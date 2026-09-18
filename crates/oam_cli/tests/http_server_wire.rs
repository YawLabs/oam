//! The http server as a raw TCP client sees it: the connection addresses a
//! request reports, and the request heads the server refuses before a
//! handler runs.
//!
//! Every client here is a plain socket driven from Rust, so what is on the
//! wire is exactly what the test wrote -- no client library normalises a
//! request head or picks the source address.

use std::io::{BufRead, BufReader};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

fn write_temp(name: &str, content: &str) -> PathBuf {
    use std::sync::OnceLock;
    static RUN_DIR: OnceLock<PathBuf> = OnceLock::new();
    let dir = RUN_DIR.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("oam-http-wire-{}-{nanos}", std::process::id()))
    });
    let path = dir.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, content).unwrap();
    path
}

/// A running `oam run` server script. The script prints `PORT <n>` once it
/// listens and then one line per event it wants the test to see; the lines
/// are forwarded to `lines`. Dropping it kills the process.
struct Server {
    child: Child,
    port: u16,
    lines: mpsc::Receiver<String>,
}

impl Server {
    fn start(name: &str, source: &str, args: &[&str], env: &[(&str, &str)]) -> Server {
        let script = write_temp(name, source);
        let cache = write_temp("oam-cache/.keep", "")
            .parent()
            .unwrap()
            .to_path_buf();
        // OAM_WIRE_TEST_BIN runs the suite against another oam build -- how
        // each test here was shown to fail on the release before its fix.
        let bin = std::env::var("OAM_WIRE_TEST_BIN")
            .unwrap_or_else(|_| env!("CARGO_BIN_EXE_oam").to_string());
        let mut cmd = Command::new(bin);
        cmd.args(args)
            .args(["run", script.to_str().unwrap(), "--no-check"])
            .env("OAM_CACHE_DIR", cache)
            .env_remove("NODE_OPTIONS")
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("oam runs");
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let first = rx
            .recv_timeout(Duration::from_secs(60))
            .expect("the server script prints its port");
        let port = first
            .strip_prefix("PORT ")
            .unwrap_or_else(|| panic!("expected `PORT <n>`, got {first:?}"))
            .trim()
            .parse()
            .unwrap();
        Server {
            child,
            port,
            lines: rx,
        }
    }

    /// The next line the script printed, or None if none arrives in time.
    fn next_line(&self, wait: Duration) -> Option<String> {
        self.lines.recv_timeout(wait).ok()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// What a raw exchange produced.
struct Exchange {
    /// Everything the server sent, as latin1.
    response: String,
    /// The server closed the connection (EOF or reset) before the deadline.
    closed: bool,
    /// The client socket's own address.
    local: SocketAddr,
}

impl Exchange {
    fn statuses(&self) -> Vec<String> {
        self.response
            .split("\r\n")
            .filter(|l| l.starts_with("HTTP/1."))
            .map(|l| l.to_string())
            .collect()
    }
}

/// Connect to `target` (from `local` when given), write `bytes`, and read
/// until the server closes or `wait` passes.
fn exchange(target: SocketAddr, local: Option<IpAddr>, bytes: &[u8], wait: Duration) -> Exchange {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let socket = match target {
            SocketAddr::V4(_) => tokio::net::TcpSocket::new_v4().unwrap(),
            SocketAddr::V6(_) => tokio::net::TcpSocket::new_v6().unwrap(),
        };
        if let Some(ip) = local {
            socket
                .bind(SocketAddr::new(ip, 0))
                .expect("bind the client address");
        }
        let mut stream = socket.connect(target).await.expect("connect");
        let local = stream.local_addr().unwrap();
        stream.write_all(bytes).await.unwrap();
        let mut response = Vec::new();
        let mut closed = false;
        let deadline = tokio::time::Instant::now() + wait;
        let mut buf = [0u8; 65536];
        loop {
            match tokio::time::timeout_at(deadline, stream.read(&mut buf)).await {
                Ok(Ok(0)) | Ok(Err(_)) => {
                    closed = true;
                    break;
                }
                Ok(Ok(n)) => response.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
        Exchange {
            response: response.iter().map(|&b| b as char).collect(),
            closed,
            local,
        }
    })
}

/// Prints `PORT <n>`, then one JSON line per request / upgrade with what the
/// server saw about the connection. HOST picks the listen address.
const ADDR_SERVER: &str = r#"
import http from "node:http";
const fields = (s) => ({
  remoteAddress: s.remoteAddress, remotePort: s.remotePort, remoteFamily: s.remoteFamily,
  localAddress: s.localAddress, localPort: s.localPort, localFamily: s.localFamily,
});
const server = http.createServer((req, res) => {
  console.log(JSON.stringify({ kind: "request", url: req.url, ...fields(req.socket), connection: req.connection === req.socket }));
  res.end("ok");
});
server.on("upgrade", (req, socket) => {
  console.log(JSON.stringify({ kind: "upgrade", url: req.url, ...fields(socket), same: req.socket === socket }));
  socket.end("HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: x\r\n\r\n");
});
server.listen(0, process.env.HOST, () => console.log("PORT " + server.address().port));
"#;

fn json(line: &str) -> serde_json::Value {
    serde_json::from_str(line).unwrap_or_else(|e| panic!("not JSON ({e}): {line}"))
}

/// A client that is not 127.0.0.1 is reported as itself, never as loopback's
/// first address -- for a request and for an upgrade. The server listens on
/// 127.0.0.1 and the client binds 127.0.0.2, which Windows and Linux route
/// over loopback (macOS configures only 127.0.0.1).
#[cfg(any(windows, target_os = "linux"))]
#[test]
fn a_request_reports_the_clients_own_address() {
    let server = Server::start("addr_v4.mjs", ADDR_SERVER, &[], &[("HOST", "127.0.0.1")]);
    let target: SocketAddr = format!("127.0.0.1:{}", server.port).parse().unwrap();
    let client: IpAddr = "127.0.0.2".parse().unwrap();

    let ex = exchange(
        target,
        Some(client),
        b"GET /a HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        Duration::from_secs(10),
    );
    assert_eq!(ex.statuses(), ["HTTP/1.1 200 OK"], "{}", ex.response);
    let seen = json(
        &server
            .next_line(Duration::from_secs(10))
            .expect("request line"),
    );
    assert_eq!(seen["kind"], "request");
    assert_eq!(seen["remoteAddress"], "127.0.0.2", "{seen}");
    assert_eq!(seen["remotePort"], ex.local.port(), "{seen}");
    assert_eq!(seen["remoteFamily"], "IPv4");
    assert_eq!(seen["localAddress"], "127.0.0.1");
    assert_eq!(seen["localPort"], server.port);
    assert_eq!(seen["localFamily"], "IPv4");
    assert_eq!(seen["connection"], true);

    let ex = exchange(
        target,
        Some(client),
        b"GET /u HTTP/1.1\r\nHost: x\r\nConnection: Upgrade\r\nUpgrade: x\r\n\r\n",
        Duration::from_secs(10),
    );
    assert_eq!(
        ex.statuses(),
        ["HTTP/1.1 101 Switching Protocols"],
        "{}",
        ex.response
    );
    let seen = json(
        &server
            .next_line(Duration::from_secs(10))
            .expect("upgrade line"),
    );
    assert_eq!(seen["kind"], "upgrade");
    assert_eq!(seen["remoteAddress"], "127.0.0.2", "{seen}");
    assert_eq!(seen["remotePort"], ex.local.port(), "{seen}");
    assert_eq!(seen["remoteFamily"], "IPv4");
    assert_eq!(seen["localAddress"], "127.0.0.1");
    assert_eq!(seen["localPort"], server.port);
    assert_eq!(seen["same"], true);
}

/// An IPv6 client is reported as IPv6: before, a request said `127.0.0.1`
/// and an upgrade socket said family `IPv4` for a `::1` client.
#[test]
fn an_ipv6_client_is_reported_as_ipv6() {
    let server = Server::start("addr_v6.mjs", ADDR_SERVER, &[], &[("HOST", "::1")]);
    let target: SocketAddr = format!("[::1]:{}", server.port).parse().unwrap();
    for (path, head) in [
        (
            "request",
            "GET /a HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        ),
        (
            "upgrade",
            "GET /u HTTP/1.1\r\nHost: x\r\nConnection: Upgrade\r\nUpgrade: x\r\n\r\n",
        ),
    ] {
        let ex = exchange(target, None, head.as_bytes(), Duration::from_secs(10));
        assert!(ex.closed, "{path}: {}", ex.response);
        let seen = json(&server.next_line(Duration::from_secs(10)).expect("a line"));
        assert_eq!(seen["kind"], path);
        assert_eq!(seen["remoteAddress"], "::1", "{seen}");
        assert_eq!(seen["remotePort"], ex.local.port(), "{seen}");
        assert_eq!(seen["remoteFamily"], "IPv6", "{seen}");
        assert_eq!(seen["localAddress"], "::1", "{seen}");
        assert_eq!(seen["localFamily"], "IPv6", "{seen}");
    }
}

/// An IPv4 client of a dual-stack `::` listener keeps its v4-mapped IPv6
/// spelling and family, as node reports it (`::ffff:127.0.0.1`, `IPv6`).
/// oam's `::` listener is IPv6-only on Windows, and a Linux host can be set
/// to `bindv6only`, so the test only asserts when the IPv4 connect succeeds.
#[cfg(unix)]
#[test]
fn an_ipv4_client_of_a_dual_stack_listener_stays_v4_mapped() {
    let server = Server::start("addr_dual.mjs", ADDR_SERVER, &[], &[("HOST", "::")]);
    let target: SocketAddr = format!("127.0.0.1:{}", server.port).parse().unwrap();
    if std::net::TcpStream::connect(target).is_err() {
        eprintln!("skipped: this host's `::` listener is IPv6-only");
        return;
    }
    let _ = server.next_line(Duration::from_millis(500)); // the probe connect
    let ex = exchange(
        target,
        None,
        b"GET /a HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        Duration::from_secs(10),
    );
    assert_eq!(ex.statuses(), ["HTTP/1.1 200 OK"], "{}", ex.response);
    let seen = json(
        &server
            .next_line(Duration::from_secs(10))
            .expect("request line"),
    );
    assert_eq!(seen["remoteAddress"], "::ffff:127.0.0.1", "{seen}");
    assert_eq!(seen["remotePort"], ex.local.port(), "{seen}");
    assert_eq!(seen["remoteFamily"], "IPv6", "{seen}");
    assert_eq!(seen["localAddress"], "::ffff:127.0.0.1", "{seen}");
    assert_eq!(seen["localFamily"], "IPv6", "{seen}");
}

/// A connection that has sent nothing yet does not hold up the next client.
/// The accept loop used to wait, inline, for each new connection's first
/// bytes (to spot an upgrade), so one silent connection -- a browser's
/// preconnect, or anyone -- stopped the server from accepting at all.
#[test]
fn a_silent_connection_does_not_block_the_next_client() {
    let server = Server::start("idle.mjs", ADDR_SERVER, &[], &[("HOST", "127.0.0.1")]);
    let target: SocketAddr = format!("127.0.0.1:{}", server.port).parse().unwrap();
    let _silent = std::net::TcpStream::connect(target).expect("connect");
    let ex = exchange(
        target,
        None,
        b"GET /next HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        Duration::from_secs(5),
    );
    assert_eq!(ex.statuses(), ["HTTP/1.1 200 OK"], "{:?}", ex.response);
    let seen = json(
        &server
            .next_line(Duration::from_secs(5))
            .expect("request line"),
    );
    assert_eq!(seen["url"], "/next");
}
