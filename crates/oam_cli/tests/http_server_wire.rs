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
        // A server that refuses the head early may close while the rest is
        // still being written; what it answered is still read below.
        let _ = stream.write_all(bytes).await;
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

/// Nothing arrives at the script within `wait`.
fn assert_no_more_lines(server: &Server, wait: Duration) {
    if let Some(line) = server.next_line(wait) {
        panic!("the server handled something it should have refused: {line}");
    }
}

/// A request carrying both Content-Length and Transfer-Encoding is refused
/// before any handler runs, and the connection is closed -- so bytes the two
/// headers disagree about are never read as a second request. (Before, the
/// handler saw both headers, and the bytes after the chunked body were
/// served as a request of their own.)
#[test]
fn a_request_with_two_body_lengths_is_refused_and_ends_the_connection() {
    let server = Server::start("clte.mjs", ADDR_SERVER, &[], &[("HOST", "127.0.0.1")]);
    let target: SocketAddr = format!("127.0.0.1:{}", server.port).parse().unwrap();
    let ex = exchange(
        target,
        None,
        b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 4\r\nTransfer-Encoding: chunked\r\n\r\n\
          0\r\n\r\nGET /second HTTP/1.1\r\nHost: x\r\n\r\n",
        Duration::from_secs(5),
    );
    assert_eq!(
        ex.statuses(),
        ["HTTP/1.1 400 Bad Request"],
        "{:?}",
        ex.response
    );
    assert!(ex.closed, "the connection must close after the refusal");
    assert_no_more_lines(&server, Duration::from_millis(500));

    // The same head as an upgrade request goes through the same rules.
    let ex = exchange(
        target,
        None,
        b"GET /up HTTP/1.1\r\nHost: x\r\nConnection: Upgrade\r\nUpgrade: x\r\n\
          Content-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n",
        Duration::from_secs(5),
    );
    assert_eq!(
        ex.statuses(),
        ["HTTP/1.1 400 Bad Request"],
        "{:?}",
        ex.response
    );
    assert!(ex.closed);
    assert_no_more_lines(&server, Duration::from_millis(500));
}

/// A head over node's 16 KiB default is answered 431 without running a
/// handler; a head that never ends is answered 431 once it outgrows the
/// read buffer (64 KiB for the default limit), not buffered on and on.
#[test]
fn an_oversized_head_is_answered_431() {
    let server = Server::start("big.mjs", ADDR_SERVER, &[], &[("HOST", "127.0.0.1")]);
    let target: SocketAddr = format!("127.0.0.1:{}", server.port).parse().unwrap();
    let head = format!(
        "GET /big HTTP/1.1\r\nHost: x\r\nX-A: {}\r\n\r\n",
        "a".repeat(20_000)
    );
    let ex = exchange(target, None, head.as_bytes(), Duration::from_secs(5));
    assert_eq!(
        ex.statuses(),
        ["HTTP/1.1 431 Request Header Fields Too Large"],
        "{:?}",
        ex.response
    );
    assert!(ex.closed);
    assert_no_more_lines(&server, Duration::from_millis(300));

    let endless = format!(
        "GET /endless HTTP/1.1\r\nHost: x\r\nX-A: {}",
        "a".repeat(300_000)
    );
    let ex = exchange(target, None, endless.as_bytes(), Duration::from_secs(5));
    assert_eq!(
        ex.statuses(),
        ["HTTP/1.1 431 Request Header Fields Too Large"],
        "{:?}",
        &ex.response[..ex.response.len().min(200)]
    );
    assert_no_more_lines(&server, Duration::from_millis(300));
}

/// `--max-http-header-size` sets the limit for servers that set none, from
/// the command line and from NODE_OPTIONS; `--insecure-http-parser` lets a
/// Content-Length + Transfer-Encoding request through, as node does.
#[test]
fn the_parser_flags_apply_to_servers() {
    let head = format!(
        "GET /mid HTTP/1.1\r\nHost: x\r\nConnection: close\r\nX-A: {}\r\n\r\n",
        "a".repeat(3000)
    );
    for (args, env) in [
        (
            &["--max-http-header-size=2000"][..],
            &[("HOST", "127.0.0.1")][..],
        ),
        (
            &[][..],
            &[
                ("HOST", "127.0.0.1"),
                ("NODE_OPTIONS", "--max-http-header-size=2000"),
            ][..],
        ),
    ] {
        let server = Server::start("flag.mjs", ADDR_SERVER, args, env);
        let target: SocketAddr = format!("127.0.0.1:{}", server.port).parse().unwrap();
        let ex = exchange(target, None, head.as_bytes(), Duration::from_secs(5));
        assert_eq!(
            ex.statuses(),
            ["HTTP/1.1 431 Request Header Fields Too Large"],
            "{args:?} {env:?}: {:?}",
            ex.response
        );
    }
    // Without the flag the same head is fine -- and so it is with an empty
    // value in NODE_OPTIONS, which is ignored rather than read as 0 (a limit
    // that would refuse every request).
    for env in [
        &[("HOST", "127.0.0.1")][..],
        &[
            ("HOST", "127.0.0.1"),
            ("NODE_OPTIONS", "--max-http-header-size="),
        ][..],
    ] {
        let server = Server::start("noflag.mjs", ADDR_SERVER, &[], env);
        let target: SocketAddr = format!("127.0.0.1:{}", server.port).parse().unwrap();
        let ex = exchange(target, None, head.as_bytes(), Duration::from_secs(5));
        assert_eq!(
            ex.statuses(),
            ["HTTP/1.1 200 OK"],
            "{env:?}: {:?}",
            ex.response
        );
    }

    let server = Server::start(
        "insecure.mjs",
        ADDR_SERVER,
        &["--insecure-http-parser"],
        &[("HOST", "127.0.0.1")],
    );
    let target: SocketAddr = format!("127.0.0.1:{}", server.port).parse().unwrap();
    let ex = exchange(
        target,
        None,
        b"POST /lenient HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Length: 5\r\n\
          Transfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n",
        Duration::from_secs(5),
    );
    assert_eq!(ex.statuses(), ["HTTP/1.1 200 OK"], "{:?}", ex.response);
    let seen = json(
        &server
            .next_line(Duration::from_secs(5))
            .expect("request line"),
    );
    assert_eq!(seen["url"], "/lenient");
}

/// A one-shot raw HTTP server on 127.0.0.1 that answers every connection
/// with `response` and closes. Returns its port.
fn raw_responder(response: Vec<u8>) -> u16 {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(&response);
        }
    });
    port
}

/// The response-head limit follows `--max-http-header-size` too: a 20 KiB
/// response header fails `fetch` by default (node: `UND_ERR_HEADERS_OVERFLOW`)
/// and is accepted under a 32 KiB limit.
#[test]
fn the_response_head_limit_follows_the_flag() {
    let mut response = b"HTTP/1.1 200 OK\r\nX-A: ".to_vec();
    response.extend(std::iter::repeat_n(b'a', 20_000));
    response.extend_from_slice(b"\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
    let port = raw_responder(response);
    let script = write_temp(
        "fetch_limit.mjs",
        "try { const r = await fetch(`http://127.0.0.1:${process.env.PORT}/`); \
         console.log('status', r.status, await r.text()); } \
         catch (e) { console.log('rejects', e.message, e.cause && e.cause.code); }",
    );
    let run = |flags: &[&str]| {
        let bin = std::env::var("OAM_WIRE_TEST_BIN")
            .unwrap_or_else(|_| env!("CARGO_BIN_EXE_oam").to_string());
        let out = Command::new(bin)
            .args(flags)
            .args(["run", script.to_str().unwrap(), "--no-check"])
            .env("PORT", port.to_string())
            .env_remove("NODE_OPTIONS")
            .output()
            .expect("oam runs");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    assert_eq!(
        run(&[]),
        "rejects fetch failed UND_ERR_HEADERS_OVERFLOW",
        "the default limit"
    );
    assert_eq!(
        run(&["--max-http-header-size=32768"]),
        "status 200 ok",
        "a raised limit"
    );
}

/// `http.request` holds a response head to the flag's limit on both of its
/// paths -- oam's own transport and the socket an agent's `createConnection`
/// returns -- failing with node's own `HPE_HEADER_OVERFLOW` error (measured
/// on node v22.22.2 with the same flags). A 20 KiB header fails under the
/// default limit and passes under a 32 KiB one.
#[test]
fn the_response_head_limit_follows_the_flag_on_both_http_request_paths() {
    let mut response = b"HTTP/1.1 200 OK\r\nX-A: ".to_vec();
    response.extend(std::iter::repeat_n(b'a', 20_000));
    response.extend_from_slice(b"\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
    let port = raw_responder(response);
    let script = write_temp(
        "http_request_limit.mjs",
        "import http from 'node:http';\n\
         import net from 'node:net';\n\
         class OwnSocket extends http.Agent {\n\
           createConnection(options, cb) { return net.createConnection(options, cb); }\n\
         }\n\
         const get = (agent) => new Promise((resolve) => {\n\
           const req = http.get({ host: '127.0.0.1', port: +process.env.PORT, path: '/', agent }, (res) => {\n\
             let body = '';\n\
             res.on('data', (d) => (body += d));\n\
             res.on('end', () => resolve(`status ${res.statusCode} ${body}`));\n\
           });\n\
           req.on('error', (e) => resolve(`error ${e.code} ${e.message}`));\n\
         });\n\
         console.log('own transport', await get(false));\n\
         console.log('agent socket', await get(new OwnSocket()));\n",
    );
    let run = |flags: &[&str]| {
        let bin = std::env::var("OAM_WIRE_TEST_BIN")
            .unwrap_or_else(|_| env!("CARGO_BIN_EXE_oam").to_string());
        let out = Command::new(bin)
            .args(flags)
            .args(["run", script.to_str().unwrap(), "--no-check"])
            .env("PORT", port.to_string())
            .env_remove("NODE_OPTIONS")
            .output()
            .expect("oam runs");
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .replace("\r\n", "\n")
    };
    assert_eq!(
        run(&[]),
        "own transport error HPE_HEADER_OVERFLOW Parse Error: Header overflow\n\
         agent socket error HPE_HEADER_OVERFLOW Parse Error: Header overflow",
        "the default limit"
    );
    assert_eq!(
        run(&["--max-http-header-size=32768"]),
        "own transport status 200 ok\nagent socket status 200 ok",
        "a raised limit"
    );
}

/// Reads each request body to the end before answering, and reports how the
/// body ended. Has no 'upgrade' listener.
const BODY_SERVER: &str = r#"
import http from "node:http";
const server = http.createServer((req, res) => {
  let body = "";
  req.on("data", (d) => (body += d));
  req.on("end", () => {
    console.log(JSON.stringify({ kind: "end", url: req.url, body }));
    res.end("ok");
  });
  req.on("error", () => console.log(JSON.stringify({ kind: "body error", url: req.url })));
});
server.listen(0, "127.0.0.1", () => console.log("PORT " + server.address().port));
"#;

/// A chunked body whose trailer section holds a bare-LF blank line fails
/// and ends the connection. hyper's trailer reader ran on to the next CRLF
/// CRLF while its trailer parse stopped at the bare-LF blank line, so the
/// bytes in between -- a whole request -- were read and dropped, and the
/// connection stayed open for more (node answers 400).
#[test]
fn a_bare_lf_in_the_trailers_fails_the_request() {
    let server = Server::start("trailer.mjs", BODY_SERVER, &[], &[]);
    let target: SocketAddr = format!("127.0.0.1:{}", server.port).parse().unwrap();
    for bytes in [
        &b"POST /t HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n\
           3\r\nabc\r\n0\r\nX: a\n\nGET /hidden HTTP/1.1\r\nHost: a\r\n\r\n"[..],
        b"POST /t HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n\
           3\r\nabc\r\n0\r\nX: a\r\n\nGET /hidden HTTP/1.1\r\nHost: a\r\n\r\n",
        b"POST /t HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n\
           3\r\nabc\r\n0\r\n\nGET /hidden HTTP/1.1\r\nHost: a\r\n\r\n",
    ] {
        let ex = exchange(target, None, bytes, Duration::from_secs(5));
        // node answers 400; the handler sees the request abort.
        assert_eq!(
            ex.statuses(),
            ["HTTP/1.1 400 Bad Request"],
            "{:?}",
            ex.response
        );
        assert!(ex.closed, "the connection must close: {:?}", ex.response);
        let seen = json(&server.next_line(Duration::from_secs(5)).expect("a line"));
        assert_eq!(seen["kind"], "body error", "{seen}");
        assert_no_more_lines(&server, Duration::from_millis(300));
    }
    // Well-formed trailers still work.
    let ex = exchange(
        target,
        None,
        b"POST /ok HTTP/1.1\r\nHost: x\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n\r\n\
          3\r\nabc\r\n0\r\nX: a\r\n\r\n",
        Duration::from_secs(5),
    );
    assert_eq!(ex.statuses(), ["HTTP/1.1 200 OK"], "{:?}", ex.response);
    let seen = json(&server.next_line(Duration::from_secs(5)).expect("a line"));
    assert_eq!(seen["body"], "abc");
}

/// An upgrade request to a server with no 'upgrade' listener is an ordinary
/// request, answered by the 'request' handler on a connection that stays
/// open, as in node. (It was taken from hyper whatever the listeners, then
/// closed unanswered -- and before that, left open for good.)
#[test]
fn an_upgrade_nobody_listens_for_is_an_ordinary_request() {
    let server = Server::start("noupgrade.mjs", BODY_SERVER, &[], &[]);
    let target: SocketAddr = format!("127.0.0.1:{}", server.port).parse().unwrap();
    let ex = exchange(
        target,
        None,
        b"GET /ws HTTP/1.1\r\nHost: x\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n",
        Duration::from_millis(800),
    );
    assert_eq!(ex.statuses(), ["HTTP/1.1 200 OK"], "{:?}", ex.response);
    assert!(!ex.closed, "the connection stays open");
    let seen = json(&server.next_line(Duration::from_secs(5)).expect("a line"));
    assert_eq!(seen["kind"], "end", "{seen}");
    assert_eq!(seen["url"], "/ws", "{seen}");
}

/// A connection handed to an 'upgrade' listener after a response the client
/// has not read yet gets all of that response first, then what the listener
/// writes: the part of it hyper still held is written out before the socket
/// changes hands, as node's socket goes on writing what it was given. (hyper
/// reads the next head while its buffer is still being written out when the
/// previous request's body ends after its response: here the handler answers
/// at once and the body comes later. Windows' loopback takes the whole
/// response into the kernel at once, so there the unit test in
/// http_server.rs, over an in-memory pipe, is the one that gets that far.)
#[test]
fn an_upgrade_after_an_unread_response_keeps_the_response() {
    use std::io::{Read, Write};
    const BIG: usize = 32 * 1024 * 1024;
    let script = format!(
        r#"
import http from "node:http";
const big = Buffer.alloc({BIG}, 0x61);
const server = http.createServer((req, res) => res.end(big));
server.on("upgrade", (req, socket, head) => {{
  socket.end("HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: x\r\n\r\n");
}});
server.listen(0, "127.0.0.1", () => console.log("PORT " + server.address().port));
"#
    );
    let server = Server::start("upgrade_after_big.mjs", &script, &[], &[]);
    let target: SocketAddr = format!("127.0.0.1:{}", server.port).parse().unwrap();
    let mut stream = std::net::TcpStream::connect(target).unwrap();
    stream
        .write_all(b"POST /big HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\n")
        .unwrap();
    // The response fills the socket buffers while this client reads nothing;
    // then the body ends, and the upgrade request comes.
    std::thread::sleep(Duration::from_millis(400));
    stream
        .write_all(b"helloGET /ws HTTP/1.1\r\nHost: x\r\nConnection: Upgrade\r\nUpgrade: x\r\n\r\n")
        .unwrap();
    std::thread::sleep(Duration::from_millis(400));
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let mut all = Vec::new();
    let _ = stream.read_to_end(&mut all);
    let head_end = all
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a response head")
        + 4;
    let head = String::from_utf8_lossy(&all[..head_end]).to_lowercase();
    assert!(head.starts_with("http/1.1 200 ok"), "{head}");
    assert!(head.contains(&format!("content-length: {BIG}")), "{head}");
    let body = &all[head_end..(head_end + BIG).min(all.len())];
    assert_eq!(body.len(), BIG, "the whole body arrives");
    assert!(
        body.iter().all(|&b| b == b'a'),
        "and nothing else inside it"
    );
    let rest = String::from_utf8_lossy(&all[head_end + BIG..]);
    assert!(
        rest.starts_with("HTTP/1.1 101 Switching Protocols\r\n"),
        "then the listener's answer: {rest:?}"
    );
}

/// Whitespace after a chunk size -- which parsers disagree about -- is
/// refused with node's 400 and the connection closes; so is any other chunk
/// size line node refuses, and chunk extensions over the limit get 413. The
/// handler, already running, sees its request body fail. (hyper took the
/// whitespace as part of the size line, and a malformed body used to close
/// the connection without a status.)
#[test]
fn a_malformed_chunk_size_line_is_answered_like_node() {
    let server = Server::start("chunksize.mjs", BODY_SERVER, &[], &[]);
    let target: SocketAddr = format!("127.0.0.1:{}", server.port).parse().unwrap();
    let head = "POST /c HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n";
    let long_extension = format!("3;e={}\r\nabc\r\n0\r\n\r\n", "a".repeat(20_000));
    for (body, status) in [
        ("3 \r\nabc\r\n0\r\n\r\n", "HTTP/1.1 400 Bad Request"),
        ("3\t\r\nabc\r\n0\r\n\r\n", "HTTP/1.1 400 Bad Request"),
        ("3 ;e=1\r\nabc\r\n0\r\n\r\n", "HTTP/1.1 400 Bad Request"),
        ("3\r\nabc\r\n0 \r\n\r\n", "HTTP/1.1 400 Bad Request"),
        ("zz\r\nabc\r\n0\r\n\r\n", "HTTP/1.1 400 Bad Request"),
        (long_extension.as_str(), "HTTP/1.1 413 Payload Too Large"),
    ] {
        let request = format!("{head}{body}");
        let ex = exchange(target, None, request.as_bytes(), Duration::from_secs(5));
        assert_eq!(ex.statuses(), [status], "{body:?}: {:?}", ex.response);
        assert!(ex.closed, "{body:?}: the connection must close");
        let seen = json(&server.next_line(Duration::from_secs(5)).expect("a line"));
        assert_eq!(seen["kind"], "body error", "{body:?}: {seen}");
    }
    // A well-formed chunked body with an extension is still read.
    let ex = exchange(
        target,
        None,
        format!("{head}3;e=1\r\nabc\r\n0\r\n\r\n").as_bytes(),
        Duration::from_millis(500),
    );
    assert_eq!(ex.statuses(), ["HTTP/1.1 200 OK"], "{:?}", ex.response);
    let seen = json(&server.next_line(Duration::from_secs(5)).expect("a line"));
    assert_eq!(seen["body"], "abc");
}

/// A server with short node timeouts: headersTimeout 400 ms, requestTimeout
/// 800 ms, keepAliveTimeout 300 ms with no buffer, checked every 50 ms.
const TIMEOUT_SERVER: &str = r#"
import http from "node:http";
const server = http.createServer(
  { headersTimeout: 400, requestTimeout: 800, keepAliveTimeout: 300, keepAliveTimeoutBuffer: 0, connectionsCheckingInterval: 50 },
  (req, res) => res.end("ok"),
);
server.listen(0, "127.0.0.1", () => console.log("PORT " + server.address().port));
"#;

/// More connections than oam's servers used to take at once (256).
const FLOOD: usize = 300;

/// Descriptors a flood test needs beyond its `FLOOD` sockets: the server's
/// pipes, the `served_within` probe, and the rest of the binary's slack.
const FLOOD_SLACK: usize = 64;

/// The flood tests hold `FLOOD` sockets each for the length of the test, and
/// `cargo test` runs them on threads of ONE process, so unserialised they ask
/// for five times the descriptors at once. Take this first and they queue
/// instead, which is also what makes the budget check below a fixed number.
static FLOOD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// How many descriptors this process may open at once -- the soft
/// `RLIMIT_NOFILE`, as a shell's `ulimit -n` reports it -- or `None` where
/// there is no such limit (Windows) or it cannot be read. It is read through
/// a shell because std exposes no rlimit call and this crate cannot make one
/// without `unsafe`: `conformance/unsafe-budget.json` is a ratchet that only
/// goes down, and a test convenience is no reason to spend from it.
#[cfg(unix)]
fn fd_budget() -> Option<usize> {
    let out = Command::new("/bin/sh")
        .arg("-c")
        .arg("ulimit -n")
        .output()
        .ok()?;
    let reported = String::from_utf8_lossy(&out.stdout);
    let reported = reported.trim();
    if reported == "unlimited" {
        return Some(usize::MAX);
    }
    reported.parse().ok()
}

#[cfg(not(unix))]
fn fd_budget() -> Option<usize> {
    None
}

/// Serialise a flood test, or tell the caller to skip it.
///
/// A flood test dials `FLOOD` sockets from THIS process and the server
/// answers every one, so the harness needs the descriptors for them: a stock
/// macOS login shell allows 256, no more than the 256 connections the server
/// used to stop at, so the harness -- not the server -- is what runs out
/// first, and `> 256` could never hold however well the server behaved. That
/// arrives as an `EMFILE` inside a connect, or as `113 connections`, both of
/// which read like the server failing. Say so and skip instead, the way the
/// dual-stack test skips a host whose `::` listener is IPv6-only.
/// `scripts/ci-local.sh` and `scripts/build-remote.sh` raise the limit before
/// `cargo test`, so the gates that must run these tests do run them.
#[must_use]
fn flood_guard() -> Option<std::sync::MutexGuard<'static, ()>> {
    let needed = FLOOD + FLOOD_SLACK;
    if let Some(budget) = fd_budget()
        && budget < needed
    {
        eprintln!(
            "skipped: this process may open {budget} descriptors and this test needs {needed} \
             -- raise `ulimit -n` (the release legs do)"
        );
        return None;
    }
    // A flood test that panicked poisoned the lock; the next one still wants
    // to run, and its own assertions are what judge it.
    Some(FLOOD_LOCK.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Keep trying a request until one is answered 200 or `deadline` passes.
fn served_within(target: SocketAddr, deadline: Duration) -> Option<Duration> {
    let started = std::time::Instant::now();
    while started.elapsed() < deadline {
        let ex = exchange(
            target,
            None,
            b"GET /real HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
            Duration::from_millis(500),
        );
        if ex
            .statuses()
            .first()
            .is_some_and(|s| s == "HTTP/1.1 200 OK")
        {
            return Some(started.elapsed());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

/// Connections that never send a request are answered 408 and closed once
/// headersTimeout passes, so a flood of them locks a server out only until
/// then. (They were held for good: past the server's connection limit every
/// new client was dropped for as long as they stayed open.)
#[test]
fn silent_connections_do_not_lock_the_server_out() {
    use std::io::Read;
    let Some(_flood) = flood_guard() else { return };
    let server = Server::start("silent_flood.mjs", TIMEOUT_SERVER, &[], &[]);
    let target: SocketAddr = format!("127.0.0.1:{}", server.port).parse().unwrap();
    let silent: Vec<std::net::TcpStream> = (0..FLOOD)
        .map(|_| std::net::TcpStream::connect(target).expect("connect"))
        .collect();
    let waited = served_within(target, Duration::from_secs(8));
    assert!(
        waited.is_some(),
        "a real client must be served once headersTimeout closed the silent connections"
    );
    // The silent connections were answered as node answers them.
    let mut first = &silent[0];
    first
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut answer = Vec::new();
    let _ = first.read_to_end(&mut answer);
    assert_eq!(
        String::from_utf8_lossy(&answer),
        "HTTP/1.1 408 Request Timeout\r\nConnection: close\r\n\r\n"
    );
}

/// Idle keep-alive connections are closed once keepAliveTimeout (plus its
/// buffer) passes, so a client pool that keeps its connections open does
/// not lock others out. (They were held for good.)
#[test]
fn idle_keep_alive_connections_do_not_lock_the_server_out() {
    use std::io::{Read, Write};
    let Some(_flood) = flood_guard() else { return };
    let server = Server::start("idle_flood.mjs", TIMEOUT_SERVER, &[], &[]);
    let target: SocketAddr = format!("127.0.0.1:{}", server.port).parse().unwrap();
    let mut idle = Vec::new();
    for _ in 0..FLOOD {
        let Ok(mut stream) = std::net::TcpStream::connect(target) else {
            break;
        };
        // One request each, answered, then nothing more: an idle pool.
        if stream
            .write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
            .is_err()
        {
            break;
        }
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut buf = [0u8; 256];
        let _ = stream.read(&mut buf);
        idle.push(stream);
    }
    assert!(!idle.is_empty());
    let waited = served_within(target, Duration::from_secs(8));
    assert!(
        waited.is_some(),
        "a real client must be served once keepAliveTimeout closed the idle connections"
    );
    // The idle connections were closed without another byte.
    let mut first = &idle[0];
    let mut rest = Vec::new();
    let _ = first.read_to_end(&mut rest);
    assert!(rest.is_empty(), "{:?}", String::from_utf8_lossy(&rest));
}

/// A request pipelined behind one whose body the handler never read is held
/// to its own timeouts: the end of the first body, read after the second
/// head was parsed, used to count as the second request's end, so a second
/// request whose body never came was never answered 408 and its connection
/// stayed open for good.
#[test]
fn a_pipelined_request_after_an_unread_body_keeps_its_timeouts() {
    use std::io::{Read, Write};
    // Answers /a without reading its body; /x waits for its body.
    let script = TIMEOUT_SERVER.replace(
        r#"(req, res) => res.end("ok")"#,
        r#"(req, res) => { if (req.url === "/a") res.end("ok"); else req.on("end", () => res.end("late")).resume(); }"#,
    );
    assert_ne!(script, TIMEOUT_SERVER);
    let server = Server::start("pipelined.mjs", &script, &[], &[]);
    let target: SocketAddr = format!("127.0.0.1:{}", server.port).parse().unwrap();
    let mut stream = std::net::TcpStream::connect(target).unwrap();
    stream
        .write_all(b"POST /a HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\nab")
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    stream
        .write_all(b"cdePOST /x HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\n")
        .unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(6)))
        .unwrap();
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    let text = String::from_utf8_lossy(&response);
    // Status lines wherever they start (the first body runs into the next).
    let statuses: Vec<&str> = text
        .match_indices("HTTP/1.1 ")
        .map(|(at, _)| text[at..].split("\r\n").next().unwrap_or_default())
        .collect();
    assert_eq!(
        statuses,
        ["HTTP/1.1 200 OK", "HTTP/1.1 408 Request Timeout"],
        "{text:?}"
    );
}

/// A server with node's default timeouts (headersTimeout 60 s,
/// keepAliveTimeout 5 s + 1 s) and no maxConnections, as most are run.
const DEFAULT_SERVER: &str = r#"
import http from "node:http";
const server = http.createServer((req, res) => res.end("ok"));
server.listen(0, "127.0.0.1", () => console.log("PORT " + server.address().port));
"#;

/// Send one keep-alive request on `stream` and read its answer. False when
/// the server closed the connection or did not answer.
fn keep_alive_request(stream: &mut std::net::TcpStream) -> bool {
    use std::io::{Read, Write};
    if stream
        .write_all(b"GET /busy HTTP/1.1\r\nHost: x\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut answer = Vec::new();
    let mut buf = [0u8; 512];
    while !answer.ends_with(b"ok") {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return false,
            Ok(n) => answer.extend_from_slice(&buf[..n]),
        }
    }
    answer.starts_with(b"HTTP/1.1 200 OK")
}

/// Keep-alive clients that keep sending requests never go idle, so no
/// timeout closes them; however many there are, the next client is served
/// at once, as by node, which takes connections while the OS gives them.
/// (oam's servers stopped at 256 connections: while these stayed busy every
/// other client was turned away.)
#[test]
fn busy_keep_alive_clients_do_not_lock_the_server_out() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let Some(_flood) = flood_guard() else { return };
    let server = Server::start("busy_flood.mjs", DEFAULT_SERVER, &[], &[]);
    let target: SocketAddr = format!("127.0.0.1:{}", server.port).parse().unwrap();
    let mut busy = Vec::new();
    let mut turned_away = 0;
    for _ in 0..FLOOD {
        // A connect error is this process's own limit on open sockets, not
        // the server's doing: stop there.
        let Ok(mut stream) = std::net::TcpStream::connect(target) else {
            break;
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        if keep_alive_request(&mut stream) {
            busy.push(stream);
        } else {
            turned_away += 1;
        }
    }
    assert_eq!(turned_away, 0, "every connection is served");
    assert!(busy.len() > 256, "{} connections", busy.len());
    // Keep every one of them busy, a request a second each.
    let stop = Arc::new(AtomicBool::new(false));
    let pinger = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                for stream in busy.iter_mut() {
                    let _ = keep_alive_request(stream);
                }
                std::thread::sleep(Duration::from_secs(1));
            }
            busy
        })
    };
    let waited = served_within(target, Duration::from_secs(3));
    stop.store(true, Ordering::Relaxed);
    let busy = pinger.join().unwrap();
    assert!(
        waited.is_some(),
        "a new client must be served while {} others keep the server busy",
        busy.len()
    );
}

/// Silent connections under node's default timeouts (closed only after
/// headersTimeout, 60 s) do not keep the next client out either.
#[test]
fn silent_connections_do_not_keep_the_next_client_waiting() {
    let Some(_flood) = flood_guard() else { return };
    let server = Server::start("silent_default.mjs", DEFAULT_SERVER, &[], &[]);
    let target: SocketAddr = format!("127.0.0.1:{}", server.port).parse().unwrap();
    let silent: Vec<std::net::TcpStream> = (0..FLOOD)
        .map_while(|_| std::net::TcpStream::connect(target).ok())
        .collect();
    assert!(silent.len() > 256, "{} connections", silent.len());
    assert!(
        served_within(target, Duration::from_secs(3)).is_some(),
        "a new client must be served while {} connections say nothing",
        silent.len()
    );
}

/// The same for `http2.createServer`, which has no timeouts at all for
/// HTTP/2 (node's Http2Server): a new HTTP/2 client gets the server's
/// SETTINGS frame however many connections sit silent. (There the limit of
/// 256 locked the server for good.)
#[test]
fn silent_connections_do_not_lock_an_http2_server() {
    use std::io::{Read, Write};
    let Some(_flood) = flood_guard() else { return };
    let script = r#"
import http2 from "node:http2";
const server = http2.createServer((req, res) => res.end("ok"));
server.listen(0, "127.0.0.1", () => console.log("PORT " + server.address().port));
"#;
    let server = Server::start("h2_flood.mjs", script, &[], &[]);
    let target: SocketAddr = format!("127.0.0.1:{}", server.port).parse().unwrap();
    let silent: Vec<std::net::TcpStream> = (0..FLOOD)
        .map_while(|_| std::net::TcpStream::connect(target).ok())
        .collect();
    assert!(silent.len() > 256, "{} connections", silent.len());
    std::thread::sleep(Duration::from_millis(300));
    let mut client = std::net::TcpStream::connect(target).expect("connect");
    client
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    // The connection preface: the magic, then an empty SETTINGS frame.
    client
        .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n\x00\x00\x00\x04\x00\x00\x00\x00\x00")
        .unwrap();
    let mut frame = [0u8; 9];
    client
        .read_exact(&mut frame)
        .expect("the server answers the preface");
    assert_eq!(frame[3], 4, "a SETTINGS frame: {frame:?}");
}
