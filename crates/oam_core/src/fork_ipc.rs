//! The child end of the `child_process` IPC channel, where it cannot be left
//! to the event loop: a process that exits with messages it has not yet been
//! able to send.
//!
//! oam carries the channel on a loopback connection the child makes on first
//! use (js/node_compat.js), so a `process.send()` made before the connection
//! is up waits in a queue -- and a `process.exit()` right behind it would
//! leave with the queue unsent, where node, whose channel is a descriptor the
//! child is born holding, delivers it (measured on v22.22.2, Windows: two
//! messages then `process.exit(0)`, 3 MB, both arrive). So the queue is kept
//! here as well, and an exit that finds it non-empty sends it on a blocking
//! connection of its own before the process goes.

use std::io::{Read, Write};
use std::sync::Mutex;

struct Channel {
    /// The parent's listener and this child's token ([`set_channel`]).
    port: u16,
    token: String,
    /// The messages `process.send()` queued behind the connect, framed as
    /// they go on the wire.
    unsent: Vec<u8>,
    /// Whether the exit hook that sends `unsent` is registered.
    hooked: bool,
}

static CHANNEL: Mutex<Option<Channel>> = Mutex::new(None);

/// Record the channel this process was forked with: `<port>:<token>`, as the
/// parent put it in the environment.
pub fn set_channel(spec: &str) {
    let (port, token) = spec.split_once(':').unwrap_or((spec, ""));
    let Ok(port) = port.parse::<u16>() else {
        return;
    };
    *CHANNEL.lock().unwrap_or_else(|e| e.into_inner()) = Some(Channel {
        port,
        token: token.to_string(),
        unsent: Vec::new(),
        hooked: false,
    });
}

/// A message queued behind the channel's connect.
pub fn queue_unsent(frame: &[u8]) {
    let hook = {
        let mut guard = CHANNEL.lock().unwrap_or_else(|e| e.into_inner());
        let Some(channel) = guard.as_mut() else {
            return;
        };
        channel.unsent.extend_from_slice(frame);
        !std::mem::replace(&mut channel.hooked, true)
    };
    if hook {
        crate::register_exit_hook(send_unsent);
    }
}

/// The queue is no longer this module's to send: the connection is up and JS
/// has written it, or the channel is closed and JS has failed it.
pub fn clear_unsent() {
    if let Some(channel) = CHANNEL.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        channel.unsent = Vec::new();
    }
}

/// Send what is still queued, for a process about to exit: connect, present
/// the token, write the queue, then the close the channel gets at any exit
/// (`tcp::tcp_settle_lingering`) -- the FIN; a read to the parent's EOF for
/// at most `tcp::PEER_EOF_GRACE` (a parent that is reading closes the channel
/// at the child's end-of-stream); and a close that returns once the parent's
/// kernel holds every byte, or its bounded linger runs out
/// (`tcp::arm_delivering_close`). The write blocks, as node's channel write does
/// at exit, for as long as the queue does not fit what the kernel buffers;
/// nothing waits for a parent that is not reading a message that does fit.
/// With no parent listening there is nobody to send to, and nothing is.
pub fn send_unsent() {
    let (port, token, unsent) = {
        let mut guard = CHANNEL.lock().unwrap_or_else(|e| e.into_inner());
        let Some(channel) = guard.as_mut() else {
            return;
        };
        if channel.unsent.is_empty() {
            return;
        }
        (
            channel.port,
            channel.token.clone(),
            std::mem::take(&mut channel.unsent),
        )
    };
    let Ok(mut stream) = std::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)) else {
        return;
    };
    let sent = stream
        .write_all(format!("{token}\n").as_bytes())
        .and_then(|()| stream.write_all(&unsent));
    if sent.is_ok() && stream.shutdown(std::net::Shutdown::Write).is_ok() {
        read_to_peer_eof(
            &mut stream,
            std::time::Instant::now() + crate::tcp::PEER_EOF_GRACE,
        );
        crate::tcp::arm_delivering_close(&socket2::SockRef::from(&stream));
    }
}

/// Read and discard what the peer sends until its EOF, an error, or
/// `give_up`, whichever is first. The socket is left non-blocking.
///
/// The deadline is looked at before every read, not only when the socket has
/// nothing to give: a peer that keeps sending would otherwise hold the exit
/// for as long as it writes. (The async read to a peer's EOF in tcp.rs bounds
/// each of its reads with the same grace.)
///
/// Polled, not a read with a timeout: Winsock leaves a socket whose receive
/// timed out in a state it documents as indeterminate, and this one still has
/// an orderly close to make.
fn read_to_peer_eof(stream: &mut std::net::TcpStream, give_up: std::time::Instant) {
    if stream.set_nonblocking(true).is_err() {
        return;
    }
    let mut sink = [0u8; 4096];
    while std::time::Instant::now() < give_up {
        match stream.read(&mut sink) {
            Ok(n) if n > 0 => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            // The peer's EOF, or an error.
            _ => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::read_to_peer_eof;
    use std::io::Write;
    use std::time::{Duration, Instant};

    /// The exit-time read to the peer's EOF ends at its deadline even while
    /// the peer keeps sending. The deadline used to be consulted only when a
    /// read found nothing, so a peer that always had bytes ready held the
    /// exit for as long as it wrote.
    #[test]
    fn the_read_to_eof_is_bounded_while_the_peer_keeps_writing() {
        const PEER_WRITES_FOR: Duration = Duration::from_secs(4);
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .expect("bind a loopback listener");
        let port = listener
            .local_addr()
            .expect("the listener's address")
            .port();
        let peer = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().expect("accept the connection");
            let chunk = [0x2eu8; 4096];
            let until = Instant::now() + PEER_WRITES_FOR;
            // Ends early once the reader has gone and a write fails.
            while Instant::now() < until && conn.write_all(&chunk).is_ok() {}
        });
        let mut stream = std::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
            .expect("connect to the listener");
        let started = Instant::now();
        read_to_peer_eof(&mut stream, started + Duration::from_millis(200));
        let took = started.elapsed();
        drop(stream);
        peer.join().expect("the peer thread");
        assert!(
            took < PEER_WRITES_FOR / 2,
            "the read must stop at its deadline, not at the peer's EOF (took {took:?})"
        );
    }
}
