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
//! Three tests, because the window is a few dozen nanoseconds wide:
//!
//! * `a_send_racing_the_dispatcher_teardown_is_answered` is the race itself,
//!   on an in-memory connection, with the two threads released together.
//!   Stock hyper 1.10.1 strands about one send in a hundred here, so it
//!   fails every run.
//! * `an_h2_send_racing_the_dispatcher_teardown_is_answered` is the same race
//!   on an HTTP/2 connection, whose `ClientTask` drains the same
//!   `dispatch::Receiver`. It fails every run on stock hyper too.
//! * `a_request_racing_a_closing_pooled_connection_settles` is the shape a
//!   user meets, through oam's own transport and a real server. It hits the
//!   window far less often (stock hyper failed it in about one run in five
//!   on the Windows arm64 dev box) and stays as the end-to-end guard.

mod common;

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use bytes::Bytes;
use common::*;
use http_body_util::{BodyExt, Empty};
use hyper::client::conn::{http1, http2};
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

// ------------------------------------------------------------ the race, h2

/// What hyper's h2 client hands its executor -- the task that drives the
/// connection's I/O, and one per request for its response -- kept here and
/// polled by hand, like everything else in the race.
type Task = Pin<Box<dyn Future<Output = ()> + Send>>;

#[derive(Clone, Default)]
struct ManualExecutor {
    tasks: Arc<Mutex<Vec<Task>>>,
}

impl<F> hyper::rt::Executor<F> for ManualExecutor
where
    F: Future<Output = ()> + Send + 'static,
{
    fn execute(&self, task: F) {
        self.tasks.lock().unwrap().push(Box::pin(task));
    }
}

impl ManualExecutor {
    /// Poll every task once and forget the finished ones. A task spawned
    /// while this runs is kept for the next call.
    fn poll_all(&self) {
        let mut tasks = std::mem::take(&mut *self.tasks.lock().unwrap());
        tasks.retain_mut(|task| poll_once(task.as_mut()).is_pending());
        self.tasks.lock().unwrap().append(&mut tasks);
    }
}

const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
const H2_SETTINGS: u8 = 0x4;
const H2_HEADERS: u8 = 0x1;
const H2_GOAWAY: u8 = 0x7;

/// One HTTP/2 frame (RFC 9113 4.1).
fn h2_frame(kind: u8, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let length = (payload.len() as u32).to_be_bytes();
    let mut frame = vec![length[1], length[2], length[3], kind, flags];
    frame.extend_from_slice(&stream.to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// An in-memory connection to an HTTP/2 server: it opens with its SETTINGS,
/// acknowledges the client's, answers each request `200` with no body (a
/// HEADERS frame carrying END_STREAM), and once `eof` is set sends
/// `GOAWAY(NO_ERROR)` and closes -- a server shutting an idle connection
/// down. The frames are written out by hand, so nothing but the client
/// under test is hyper's or h2's.
struct ScriptedH2Io {
    eof: Arc<AtomicBool>,
    preface_read: bool,
    written: Vec<u8>,
    unread: Vec<u8>,
    last_stream: u32,
    goaway_sent: bool,
}

impl ScriptedH2Io {
    fn new(eof: Arc<AtomicBool>) -> Self {
        ScriptedH2Io {
            eof,
            preface_read: false,
            written: Vec::new(),
            unread: h2_frame(H2_SETTINGS, 0, 0, &[]),
            last_stream: 0,
            goaway_sent: false,
        }
    }

    /// Answer every whole frame the client has written so far.
    fn serve(&mut self) {
        if !self.preface_read {
            if self.written.len() < H2_PREFACE.len() {
                return;
            }
            assert_eq!(&self.written[..H2_PREFACE.len()], H2_PREFACE);
            self.written.drain(..H2_PREFACE.len());
            self.preface_read = true;
        }
        while self.written.len() >= 9 {
            let length = u32::from_be_bytes([0, self.written[0], self.written[1], self.written[2]]);
            let end = 9 + length as usize;
            if self.written.len() < end {
                return;
            }
            let (kind, flags) = (self.written[3], self.written[4]);
            let stream = u32::from_be_bytes([
                self.written[5] & 0x7f,
                self.written[6],
                self.written[7],
                self.written[8],
            ]);
            self.written.drain(..end);
            if kind == H2_SETTINGS && flags & 0x1 == 0 {
                self.unread
                    .extend_from_slice(&h2_frame(H2_SETTINGS, 0x1, 0, &[]));
            } else if kind == H2_HEADERS {
                // 0x88: `:status: 200` from HPACK's static table (RFC 7541
                // appendix A, index 8). 0x5: END_STREAM | END_HEADERS.
                self.last_stream = stream;
                self.unread
                    .extend_from_slice(&h2_frame(H2_HEADERS, 0x5, stream, &[0x88]));
            }
        }
    }
}

impl Read for ScriptedH2Io {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        mut buf: ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        if self.unread.is_empty() && self.eof.load(Ordering::SeqCst) && !self.goaway_sent {
            // Last stream id, then NO_ERROR.
            let mut payload = self.last_stream.to_be_bytes().to_vec();
            payload.extend_from_slice(&0u32.to_be_bytes());
            self.unread = h2_frame(H2_GOAWAY, 0, 0, &payload);
            self.goaway_sent = true;
        }
        if !self.unread.is_empty() {
            let n = self.unread.len().min(buf.remaining());
            buf.put_slice(&self.unread[..n]);
            self.unread.drain(..n);
            Poll::Ready(Ok(()))
        } else if self.goaway_sent {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }
}

impl Write for ScriptedH2Io {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.written.extend_from_slice(buf);
        self.serve();
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

type H2Connection = http2::Connection<ScriptedH2Io, Empty<Bytes>, ManualExecutor>;

fn h2_request() -> http::Request<Empty<Bytes>> {
    http::Request::builder()
        .uri("http://server.test/")
        .body(Empty::new())
        .unwrap()
}

/// The HTTP/2 twin of the race above (#184). An h2 connection's `ClientTask`
/// reads its requests from the same `dispatch::Receiver` the h1 dispatcher
/// does, so the vendored `Receiver::drop` fix covers it by construction --
/// and a hyper release that fixed only the h1 path (as hyperium/hyper#4150
/// does) would pass the h1 test and leave this one failing. The steps: a
/// connection serves a request and goes idle, the server sends
/// `GOAWAY(NO_ERROR)` and closes, the connection future finishes, and then
/// one thread drops it while another sends the next request on its
/// `SendRequest` -- which is what oam's own pool does with a pooled h2
/// connection (`http_client::pool`, `try_send_request`). Whatever the order,
/// the request must be answered.
///
/// Stock hyper 1.10.1 strands requests here as it does over h1: with the
/// drain taken out of the vendored `Receiver::drop`, this test failed 8 of 8
/// runs on Windows arm64, 18 to 37 of 20,000 requests stranded (see
/// `vendor/hyper-1.10.1/OAM-PATCH.md`).
#[test]
fn an_h2_send_racing_the_dispatcher_teardown_is_answered() {
    let barrier = Arc::new(SpinBarrier::default());
    let (to_dropper, connections) = mpsc::channel::<H2Connection>();
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
        let executor = ManualExecutor::default();
        let io = ScriptedH2Io::new(eof.clone());
        let Poll::Ready(Ok((mut sender, mut connection))) = poll_once(std::pin::pin!(
            http2::Builder::new(executor.clone()).handshake::<_, Empty<Bytes>>(io)
        )) else {
            panic!("an in-memory handshake completes at once");
        };

        // One exchange, so the connection is idle rather than never used.
        let mut first = Box::pin(sender.send_request(h2_request()));
        let mut polls = 0;
        let response = loop {
            assert!(poll_once(Pin::new(&mut connection)).is_pending());
            executor.poll_all();
            if let Poll::Ready(response) = poll_once(first.as_mut()) {
                break response.expect("the first request is answered");
            }
            polls += 1;
            assert!(polls < 8, "the first exchange did not complete");
        };
        assert_eq!(response.status(), 200);
        drop(response);
        executor.poll_all();
        assert!(poll_once(Pin::new(&mut connection)).is_pending());

        // The server's GOAWAY and FIN on the idle connection: the task
        // driving its I/O ends, and with it the connection future.
        eof.store(true, Ordering::SeqCst);
        let mut polls = 0;
        while poll_once(Pin::new(&mut connection)).is_pending() {
            executor.poll_all();
            polls += 1;
            assert!(polls < 8, "the connection did not finish on GOAWAY and FIN");
        }
        assert!(
            sender.is_ready(),
            "a finished but undropped connection still looks idle to the pool"
        );

        to_dropper.send(connection).unwrap();
        barrier.wait();
        let mut second = Box::pin(sender.try_send_request(h2_request()));
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
        "{stranded} of {ROUNDS} h2 requests sent as their connection was dropped were never \
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
