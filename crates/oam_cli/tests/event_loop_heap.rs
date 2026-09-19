//! The event loop must not keep what a turn touched alive.
//!
//! Each loop turn settles one op completion or runs one due timer. The V8
//! handles a turn creates (the promise resolver it settles and the value it
//! settles with, a timer's callback and arguments) used to land in the
//! scope of the whole loop, so nothing a callback ever touched could be
//! collected: the heap grew by about half a KiB per callback, forever, where
//! node's stays flat. Each test here measures `heapUsed` after `gc()` around
//! tens of thousands of turns and requires it to stay flat.

use std::path::PathBuf;
use std::process::Command;

fn write_temp(name: &str, content: &str) -> PathBuf {
    use std::sync::OnceLock;
    static RUN_DIR: OnceLock<PathBuf> = OnceLock::new();
    let dir = RUN_DIR.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("oam-loop-heap-{}-{nanos}", std::process::id()))
    });
    let path = dir.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, content).unwrap();
    path
}

/// Run `source` under `oam --expose-gc` and return the JSON lines it printed.
fn run_measured(name: &str, source: &str) -> Vec<serde_json::Value> {
    let script = write_temp(name, source);
    let cache = write_temp("oam-cache/.keep", "")
        .parent()
        .unwrap()
        .to_path_buf();
    // OAM_WIRE_TEST_BIN runs this against another oam build -- how these
    // tests were shown to fail before the fix.
    let bin = std::env::var("OAM_WIRE_TEST_BIN")
        .unwrap_or_else(|_| env!("CARGO_BIN_EXE_oam").to_string());
    let out = Command::new(bin)
        .args(["--expose-gc", "run", script.to_str().unwrap(), "--no-check"])
        .env("OAM_CACHE_DIR", cache)
        .env_remove("NODE_OPTIONS")
        .output()
        .expect("oam runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "exit {:?}\nstdout:\n{stdout}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|_| panic!("not JSON: {line:?}")))
        .collect()
}

/// Bytes of heap a single turn may leave behind, on average, before the test
/// calls it a leak. A turn that leaked kept about 500-700 bytes; after the
/// fix, and in node, the average is a few bytes (allocation noise over
/// 20000 turns).
const MAX_BYTES_PER_TURN: f64 = 64.0;

fn assert_flat(results: &[serde_json::Value]) {
    assert!(!results.is_empty());
    let leaking: Vec<&serde_json::Value> = results
        .iter()
        .filter(|r| r["perTurn"].as_f64().unwrap() > MAX_BYTES_PER_TURN)
        .collect();
    assert!(
        leaking.is_empty(),
        "heap kept per turn (bytes, limit {MAX_BYTES_PER_TURN}): {results:?}"
    );
}

/// Callbacks of fs ops, fs promises and due timers and immediates: every
/// kind of turn the loop runs.
#[test]
fn settled_ops_and_fired_timers_do_not_stay_on_the_heap() {
    let script = r#"
import fs from "node:fs";
const N = 20000;
const heap = () => {
  globalThis.gc();
  globalThis.gc();
  return process.memoryUsage().heapUsed;
};
async function measure(label, batch) {
  await batch(1000); // warm up: compile, feedback, lazily built objects
  const before = heap();
  await batch(N);
  const perTurn = (heap() - before) / N;
  console.log(JSON.stringify({ label, perTurn: Math.round(perTurn * 10) / 10 }));
}
const sequential = (one) => async (n) => {
  for (let i = 0; i < n; i++) await one();
};
// All n due at once, so each still fires on a turn of its own.
const together = (schedule) => (n) =>
  new Promise((resolve) => {
    let left = n;
    const done = () => {
      if (--left === 0) resolve();
    };
    for (let i = 0; i < n; i++) schedule(done);
  });
await measure("fs.stat callback", sequential(() => new Promise((r) => fs.stat(".", r))));
await measure("fs.promises.stat", sequential(() => fs.promises.stat(".")));
await measure("setTimeout", together((done) => setTimeout(done, 1)));
await measure("setImmediate", together((done) => setImmediate(done)));
"#;
    let results = run_measured("turns.mjs", script);
    assert_eq!(results.len(), 4, "{results:?}");
    assert_flat(&results);
}

/// An http server answering keep-alive requests: each request is several
/// turns (the accept or read, the body, the write), none of which may stay.
#[test]
fn a_server_does_not_keep_its_requests_on_the_heap() {
    let script = r#"
import http from "node:http";
import net from "node:net";
const heap = () => {
  globalThis.gc();
  globalThis.gc();
  return process.memoryUsage().heapUsed;
};
const server = http.createServer((req, res) => res.end("ok"));
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const { port } = server.address();
// A raw keep-alive client, so the client side adds as little as possible:
// one request, wait for its answer, the next.
const connect = () =>
  new Promise((resolve) => {
    const socket = net.connect(port, "127.0.0.1", () => resolve(socket));
    socket.setEncoding("latin1");
    socket.answer = "";
    socket.on("data", (chunk) => {
      socket.answer += chunk;
      if (socket.answer.endsWith("\r\n\r\nok")) {
        socket.answer = "";
        socket.answered();
      }
    });
    socket.on("error", (err) => {
      console.error(err);
      process.exit(1);
    });
  });
const connections = await Promise.all(Array.from({ length: 4 }, connect));
const request = (socket) =>
  new Promise((resolve) => {
    socket.answered = resolve;
    socket.write("GET / HTTP/1.1\r\nHost: x\r\n\r\n");
  });
const batch = (n) =>
  Promise.all(
    connections.map(async (socket) => {
      for (let i = 0; i < n / connections.length; i++) await request(socket);
    }),
  );
const N = 20000;
await batch(1000);
const before = heap();
await batch(N);
const perTurn = (heap() - before) / N;
console.log(JSON.stringify({ label: "http request", perTurn: Math.round(perTurn * 10) / 10 }));
for (const socket of connections) socket.destroy();
server.close();
"#;
    let results = run_measured("server.mjs", script);
    assert_eq!(results.len(), 1, "{results:?}");
    assert_flat(&results);
}
