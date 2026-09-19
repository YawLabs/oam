//! An in-memory byte pipe that JS pumps to and from any socket object.
//!
//! `tls.connect({ socket })` runs TLS over a stream oam did not open itself
//! -- a net.Socket to a CONNECT proxy, a TLSSocket (TLS in TLS), or any JS
//! Duplex. An `http2.connect` session runs over one the same way (hyper's h2
//! client over the socket `net.connect`, `tls.connect` or `createConnection`
//! returned; `http_client::h2_session`), and so does a fetch whose undici
//! dispatcher has a `connect` function (over the socket that function handed
//! back; `http_client::send::fetch_supply`).
//!
//! rustls (or that consumer) runs over the pipe's near end ([`take_near`]);
//! JS owns the far end: it takes what the near end wrote with [`out`] and
//! writes it to the socket, and feeds what the socket read back with
//! [`input`], ending it with [`input_end`] at the socket's EOF. Both
//! directions are bounded by the pipe, so a peer nobody reads stops the
//! socket being read, and a slow socket stops the near end writing.
//!
//! A pipe lives until [`close`]. Closing drops JS's end, which the near end
//! reads as EOF; a parked [`out`] read returns `Done` once the near end is
//! let go of.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, ReadHalf, WriteHalf};

use crate::OpOutcome;

/// Each direction of the pipe buffers at most this much.
const PIPE: usize = 64 * 1024;

/// The most one [`out`] read returns.
const OUT_CHUNK: usize = 64 * 1024;

/// Live pipes by id (ids from the runtime's shared handle allocator).
pub type Pipes = Arc<Mutex<HashMap<u64, Pipe>>>;

/// One pipe's ends.
pub struct Pipe {
    /// The near end, until whatever runs over it takes it.
    near: Option<DuplexStream>,
    /// JS's end, reading what the near end wrote. Out of the map while a
    /// read is parked (remove-await-reinsert).
    out: Option<ReadHalf<DuplexStream>>,
    /// JS's end, writing what the socket read. Out of the map while a write
    /// is parked.
    input: Option<WriteHalf<DuplexStream>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// A new pipe; its id.
pub fn open(pipes: &Pipes, ids: &AtomicU64) -> u64 {
    let (near, far) = tokio::io::duplex(PIPE);
    let (out, input) = tokio::io::split(far);
    let id = ids.fetch_add(1, Ordering::Relaxed);
    lock(pipes).insert(
        id,
        Pipe {
            near: Some(near),
            out: Some(out),
            input: Some(input),
        },
    );
    id
}

/// The near end, once: `None` for a pipe that is gone or already taken.
pub fn take_near(pipes: &Pipes, id: u64) -> Option<DuplexStream> {
    lock(pipes).get_mut(&id).and_then(|pipe| pipe.near.take())
}

/// The next bytes the near end wrote, for the socket; `Done` at the end (the
/// near end shut down or was dropped, or the pipe closed).
pub async fn out(pipes: Pipes, id: u64) -> OpOutcome {
    let half = lock(&pipes).get_mut(&id).and_then(|pipe| pipe.out.take());
    let Some(mut half) = half else {
        return OpOutcome::Done;
    };
    let mut buf = vec![0u8; OUT_CHUNK];
    let read = half.read(&mut buf).await;
    if let Some(pipe) = lock(&pipes).get_mut(&id) {
        pipe.out = Some(half);
    }
    match read {
        Ok(0) | Err(_) => OpOutcome::Done,
        Ok(n) => {
            buf.truncate(n);
            OpOutcome::Bytes(buf)
        }
    }
}

/// Hand the near end bytes the socket read. Resolves once the pipe took them
/// all, which is the socket's read backpressure. Bytes for a closed pipe are
/// dropped.
pub async fn input(pipes: Pipes, id: u64, bytes: Vec<u8>) -> OpOutcome {
    let half = lock(&pipes).get_mut(&id).and_then(|pipe| pipe.input.take());
    let Some(mut half) = half else {
        return OpOutcome::Done;
    };
    // A near end that is gone takes nothing more: the bytes are dropped, as
    // for a closed pipe.
    let _ = half.write_all(&bytes).await;
    if let Some(pipe) = lock(&pipes).get_mut(&id) {
        pipe.input = Some(half);
    }
    OpOutcome::Done
}

/// The socket reached EOF: the near end reads the end of the stream.
pub async fn input_end(pipes: Pipes, id: u64) -> OpOutcome {
    let half = lock(&pipes).get_mut(&id).and_then(|pipe| pipe.input.take());
    if let Some(mut half) = half {
        let _ = half.shutdown().await;
    }
    OpOutcome::Done
}

/// Drop the pipe. True if it was there.
pub fn close(pipes: &Pipes, id: u64) -> bool {
    lock(pipes).remove(&id).is_some()
}

/// How many pipes are open.
pub fn open_count(pipes: &Pipes) -> usize {
    lock(pipes).len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pipes() -> (Pipes, AtomicU64) {
        (Arc::new(Mutex::new(HashMap::new())), AtomicU64::new(1))
    }

    #[tokio::test]
    async fn bytes_cross_in_both_directions_and_eof_follows_shutdown() {
        let (pipes, ids) = pipes();
        let id = open(&pipes, &ids);
        let mut near = take_near(&pipes, id).unwrap();
        assert!(
            take_near(&pipes, id).is_none(),
            "the near end is taken once"
        );

        near.write_all(b"hello").await.unwrap();
        let OpOutcome::Bytes(out_bytes) = out(pipes.clone(), id).await else {
            panic!("expected bytes");
        };
        assert_eq!(out_bytes, b"hello");

        assert!(matches!(
            input(pipes.clone(), id, b"world".to_vec()).await,
            OpOutcome::Done
        ));
        let mut buf = [0u8; 5];
        near.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"world");

        input_end(pipes.clone(), id).await;
        assert_eq!(near.read(&mut buf).await.unwrap(), 0, "EOF after input_end");

        near.shutdown().await.unwrap();
        assert!(matches!(out(pipes.clone(), id).await, OpOutcome::Done));
        assert!(close(&pipes, id));
        assert!(!close(&pipes, id));
        assert_eq!(open_count(&pipes), 0);
    }

    #[tokio::test]
    async fn closing_ends_a_parked_out_once_the_near_end_goes() {
        let (pipes, ids) = pipes();
        let id = open(&pipes, &ids);
        let near = take_near(&pipes, id).unwrap();
        let parked = tokio::spawn(out(pipes.clone(), id));
        tokio::task::yield_now().await;
        close(&pipes, id);
        drop(near);
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), parked)
            .await
            .expect("the parked read returns")
            .unwrap();
        assert!(matches!(outcome, OpOutcome::Done));
        // Bytes for a closed pipe are dropped.
        assert!(matches!(
            input(pipes.clone(), id, b"x".to_vec()).await,
            OpOutcome::Done
        ));
    }

    #[tokio::test]
    async fn closing_the_pipe_reads_as_the_peer_closing() {
        let (pipes, ids) = pipes();
        let id = open(&pipes, &ids);
        let mut near = take_near(&pipes, id).unwrap();
        assert!(close(&pipes, id));
        let mut rest = Vec::new();
        near.read_to_end(&mut rest).await.unwrap();
        assert!(rest.is_empty());
        // Nothing to pump any more.
        assert!(matches!(out(pipes.clone(), id).await, OpOutcome::Done));
    }

    #[tokio::test]
    async fn a_full_pipe_holds_the_socket_back() {
        let (pipes, ids) = pipes();
        let id = open(&pipes, &ids);
        let mut near = take_near(&pipes, id).unwrap();
        // More than the pipe holds: the write parks until the near end reads.
        let big = vec![7u8; PIPE + 1024];
        let parked = tokio::spawn(input(pipes.clone(), id, big));
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!parked.is_finished(), "input waits for the near end");
        let mut got = vec![0u8; PIPE + 1024];
        near.read_exact(&mut got).await.unwrap();
        assert!(matches!(parked.await.unwrap(), OpOutcome::Done));
        assert!(got.iter().all(|&b| b == 7));
    }
}
