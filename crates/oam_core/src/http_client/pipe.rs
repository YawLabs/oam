//! A byte pipe between a JS socket object and a Rust consumer: how an
//! `http2.connect` session runs over the socket `net.connect`, `tls.connect`
//! or the caller's `createConnection` returned, and how a fetch runs over the
//! socket an undici `Agent`'s `connect` function handed back (node's model in
//! both: the protocol runs over THAT socket, whatever connected it).
//!
//! One end of an in-memory duplex pipe is the consumer's ([`take`]): an h2
//! client connection, or a fetch connection. JS owns the other end: it takes
//! the bytes the consumer wrote with [`out`] and writes them to the socket,
//! and feeds what the socket reads back with [`input`], ending it with
//! [`input_end`] at the socket's EOF. Both directions are bounded by the pipe,
//! so a consumer that stops reading stops the socket being read, and a slow
//! socket stops the consumer writing.
//!
//! A pipe lives until [`close`] (JS closes it when the socket closes). The
//! consumer's end is independent of the entry: dropping it (the consumer is
//! done) ends a parked [`out`] with EOF, and closing the entry drops JS's
//! halves, which the consumer reads as the peer closing.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, ReadHalf, WriteHalf};

use crate::OpOutcome;

/// Each direction of the in-memory pipe buffers at most this much.
const PIPE: usize = 64 * 1024;

/// The most one [`out`] read returns.
const OUT_CHUNK: usize = 64 * 1024;

/// Open pipes by id (ids from the runtime's shared handle allocator).
pub type Pipes = Arc<Mutex<HashMap<u64, Pipe>>>;

/// One pipe's state.
pub struct Pipe {
    /// The consumer's end, until [`take`] hands it over.
    near: Option<DuplexStream>,
    /// JS's end, reading what the consumer wrote. Out of the map while a read
    /// is parked (remove-await-reinsert).
    out: Option<ReadHalf<DuplexStream>>,
    /// JS's end, writing what the socket read. Out of the map while a write
    /// is parked.
    input: Option<WriteHalf<DuplexStream>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// `socketPipeOpen`: a new pipe; the id JS pumps it with.
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

/// The consumer's end of pipe `id`, once. `None` if the pipe is gone or its
/// end was already taken.
pub fn take(pipes: &Pipes, id: u64) -> Option<DuplexStream> {
    lock(pipes).get_mut(&id).and_then(|pipe| pipe.near.take())
}

/// `socketPipeOut`: the next bytes the consumer wrote, for the socket;
/// `Done` at the end (the consumer dropped its end, or the pipe closed).
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

/// `socketPipeIn`: hand the consumer bytes the socket read. Resolves once the
/// pipe took them all, which is the socket's read backpressure. Bytes for a
/// closed pipe, or one whose consumer is gone, are dropped.
pub async fn input(pipes: Pipes, id: u64, bytes: Vec<u8>) -> OpOutcome {
    let half = lock(&pipes).get_mut(&id).and_then(|pipe| pipe.input.take());
    let Some(mut half) = half else {
        return OpOutcome::Done;
    };
    let wrote = half.write_all(&bytes).await;
    if let Some(pipe) = lock(&pipes).get_mut(&id) {
        pipe.input = Some(half);
    }
    match wrote {
        // A consumer that dropped its end reads nothing more: not the
        // socket's failure.
        Ok(()) | Err(_) => OpOutcome::Done,
    }
}

/// `socketPipeInEnd`: the socket reached EOF -- the consumer reads the end
/// of the stream.
pub async fn input_end(pipes: Pipes, id: u64) -> OpOutcome {
    let half = lock(&pipes).get_mut(&id).and_then(|pipe| pipe.input.take());
    if let Some(mut half) = half {
        let _ = half.shutdown().await;
    }
    OpOutcome::Done
}

/// `socketPipeClose`: drop the pipe (and its consumer end, if never taken).
/// True if it was there.
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

    fn pipes() -> Pipes {
        Arc::new(Mutex::new(HashMap::new()))
    }

    #[tokio::test]
    async fn bytes_flow_both_ways_and_the_consumer_dropping_ends_out() {
        let pipes = pipes();
        let ids = AtomicU64::new(1);
        let id = open(&pipes, &ids);
        let mut near = take(&pipes, id).expect("the consumer end");
        assert!(take(&pipes, id).is_none(), "taken once");

        near.write_all(b"request").await.unwrap();
        match out(pipes.clone(), id).await {
            OpOutcome::Bytes(bytes) => assert_eq!(bytes, b"request"),
            other => panic!("expected bytes, got {other:?}"),
        }

        assert!(matches!(
            input(pipes.clone(), id, b"response".to_vec()).await,
            OpOutcome::Done
        ));
        let mut buf = [0u8; 8];
        near.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"response");

        // The socket's EOF reaches the consumer as the end of the stream.
        input_end(pipes.clone(), id).await;
        let mut rest = Vec::new();
        near.read_to_end(&mut rest).await.unwrap();
        assert!(rest.is_empty());

        // The consumer is done: a parked out ends.
        drop(near);
        assert!(matches!(out(pipes.clone(), id).await, OpOutcome::Done));
        assert!(close(&pipes, id));
        assert!(!close(&pipes, id));
        assert_eq!(open_count(&pipes), 0);
    }

    #[tokio::test]
    async fn closing_the_pipe_reads_as_the_peer_closing() {
        let pipes = pipes();
        let ids = AtomicU64::new(1);
        let id = open(&pipes, &ids);
        let mut near = take(&pipes, id).unwrap();
        assert!(close(&pipes, id));
        let mut rest = Vec::new();
        near.read_to_end(&mut rest).await.unwrap();
        assert!(rest.is_empty());
        // Nothing to pump any more.
        assert!(matches!(out(pipes.clone(), id).await, OpOutcome::Done));
        assert!(matches!(
            input(pipes.clone(), id, b"x".to_vec()).await,
            OpOutcome::Done
        ));
    }

    #[tokio::test]
    async fn a_full_pipe_holds_the_socket_back() {
        let pipes = pipes();
        let ids = AtomicU64::new(1);
        let id = open(&pipes, &ids);
        let mut near = take(&pipes, id).unwrap();
        // More than the pipe holds: the write parks until the consumer reads.
        let big = vec![7u8; PIPE + 1024];
        let parked = tokio::spawn(input(pipes.clone(), id, big));
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!parked.is_finished(), "input waits for the consumer");
        let mut got = vec![0u8; PIPE + 1024];
        near.read_exact(&mut got).await.unwrap();
        assert!(matches!(parked.await.unwrap(), OpOutcome::Done));
        assert!(got.iter().all(|&b| b == 7));
    }
}
