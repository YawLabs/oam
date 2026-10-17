//! The TLS 1.3 client's last handshake flight, held until the socket is used.
//!
//! node's client reports the handshake done -- 'secureConnect' -- from
//! OpenSSL's handshake-done callback, before its TLSWrap writes out the
//! flight that callback produced: the client's Finished (with its
//! Certificate and CertificateVerify when it presents one, and the
//! compatibility ChangeCipherSpec). A socket destroyed in a 'secureConnect'
//! listener -- a certificate probe, a health check -- never sends it, so the
//! server's handshake never completes: the server sees the connection close
//! mid-handshake ('tlsClientError', ECONNRESET "socket hang up") and never
//! emits 'secureConnection'. Measured on v22.22.2 through a record-counting
//! proxy: the client sends its ClientHello, then a FIN.
//!
//! tokio-rustls flushes that flight before the connect future resolves, so
//! oam's server-side peer finished its handshake, handed out a socket, and
//! failed its first write on the reset that followed. [`HeldFlight`] keeps
//! the flight back instead: the verifier arms the gate when it accepts the
//! server's TLS 1.3 CertificateVerify -- the last message before the
//! server's Finished, after which the client writes nothing but that final
//! flight -- and the bytes written from then on are held until the first
//! read, write or shutdown on the connection releases them, in order, ahead
//! of anything written after. A connection closed first drops them.
//!
//! TLS 1.2 needs nothing: its client sends its Finished before the server's
//! and has nothing left to send when the handshake completes, so the gate is
//! never armed. Neither is it for a handshake that resumes a session (no
//! CertificateVerify) -- oam's client builds a fresh config, and with it an
//! empty session cache, per connection.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::task::{Context, Poll, ready};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

const OPEN: u8 = 0;
const ARMED: u8 = 1;
const RELEASED: u8 = 2;

/// Shared by the verifier (which arms it), the transport adapter (which
/// holds while it is armed) and the registry (whose first op on the
/// connection releases it).
#[derive(Debug, Default)]
pub(crate) struct FlightGate(AtomicU8);

impl FlightGate {
    /// The server's TLS 1.3 CertificateVerify was accepted: what the client
    /// writes from here on is its final flight. Once only: a released gate
    /// stays released.
    pub(crate) fn arm(&self) {
        let _ = self
            .0
            .compare_exchange(OPEN, ARMED, Ordering::AcqRel, Ordering::Acquire);
    }

    /// The connection is in use: the held flight goes out with whatever
    /// the transport is asked to do next.
    pub(crate) fn release(&self) {
        self.0.store(RELEASED, Ordering::Release);
    }

    fn holding(&self) -> bool {
        self.0.load(Ordering::Acquire) == ARMED
    }
}

/// A client transport whose writes are held while its gate is armed.
pub(crate) struct HeldFlight<IO> {
    io: IO,
    gate: Arc<FlightGate>,
    held: Vec<u8>,
}

impl<IO> HeldFlight<IO> {
    pub(crate) fn new(io: IO, gate: Arc<FlightGate>) -> Self {
        HeldFlight {
            io,
            gate,
            held: Vec::new(),
        }
    }

    pub(crate) fn get_ref(&self) -> &IO {
        &self.io
    }
}

impl<IO: AsyncWrite + Unpin> HeldFlight<IO> {
    /// Write out what was held, if the gate has let go of it.
    fn poll_drain(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        while !self.held.is_empty() && !self.gate.holding() {
            let n = ready!(Pin::new(&mut self.io).poll_write(cx, &self.held))?;
            if n == 0 {
                return Poll::Ready(Err(std::io::ErrorKind::WriteZero.into()));
            }
            self.held.drain(..n);
        }
        Poll::Ready(Ok(()))
    }
}

impl<IO: AsyncRead + AsyncWrite + Unpin> AsyncRead for HeldFlight<IO> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        // A connection that is only read from releases its flight here: the
        // server answers nothing until it has the client's Finished. A drain
        // still pending has registered this task for write readiness, and
        // the read below registers it for data, so either wakes it.
        if !this.held.is_empty() && !this.gate.holding() {
            if let Poll::Ready(Err(e)) = this.poll_drain(cx) {
                return Poll::Ready(Err(e));
            }
            if this.held.is_empty() {
                ready!(Pin::new(&mut this.io).poll_flush(cx))?;
            }
        }
        Pin::new(&mut this.io).poll_read(cx, buf)
    }
}

impl<IO: AsyncWrite + Unpin> AsyncWrite for HeldFlight<IO> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        if this.gate.holding() {
            this.held.extend_from_slice(buf);
            return Poll::Ready(Ok(buf.len()));
        }
        ready!(this.poll_drain(cx))?;
        Pin::new(&mut this.io).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        // The connect future's closing flush, while armed: the flight stays.
        if this.gate.holding() {
            return Poll::Ready(Ok(()));
        }
        ready!(this.poll_drain(cx))?;
        Pin::new(&mut this.io).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        // An end() is a use of the connection: the flight goes before the
        // close_notify and the FIN.
        this.gate.release();
        ready!(this.poll_drain(cx))?;
        Pin::new(&mut this.io).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn an_armed_gate_holds_writes_until_released_in_order() {
        let (near, mut far) = tokio::io::duplex(1024);
        let gate = Arc::new(FlightGate::default());
        let mut io = HeldFlight::new(near, gate.clone());

        io.write_all(b"hello ").await.unwrap();
        gate.arm();
        io.write_all(b"held ").await.unwrap();
        io.flush().await.unwrap();
        let mut buf = [0u8; 64];
        let n = far.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"hello ", "only what came before arming went");

        gate.release();
        io.write_all(b"after").await.unwrap();
        let mut got = Vec::new();
        while got.len() < b"held after".len() {
            let n = far.read(&mut buf).await.unwrap();
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got, b"held after", "the held bytes go first");
    }

    #[tokio::test]
    async fn a_read_releases_the_held_flight() {
        let (near, mut far) = tokio::io::duplex(1024);
        let gate = Arc::new(FlightGate::default());
        let mut io = HeldFlight::new(near, gate.clone());
        gate.arm();
        io.write_all(b"finished").await.unwrap();
        gate.release();

        let reader = tokio::spawn(async move {
            let mut buf = [0u8; 8];
            let n = io.read(&mut buf).await.unwrap();
            buf[..n].to_vec()
        });
        let mut buf = [0u8; 8];
        far.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"finished");
        far.write_all(b"reply").await.unwrap();
        assert_eq!(reader.await.unwrap(), b"reply");
    }

    #[tokio::test]
    async fn a_connection_dropped_while_armed_sends_nothing_held() {
        let (near, mut far) = tokio::io::duplex(1024);
        let gate = Arc::new(FlightGate::default());
        let mut io = HeldFlight::new(near, gate.clone());
        gate.arm();
        io.write_all(b"finished").await.unwrap();
        drop(io);
        let mut buf = Vec::new();
        far.read_to_end(&mut buf).await.unwrap();
        assert!(buf.is_empty(), "the flight went with the connection");
    }

    #[test]
    fn a_released_gate_is_not_rearmed() {
        let gate = FlightGate::default();
        gate.release();
        gate.arm();
        assert!(!gate.holding());
    }
}
