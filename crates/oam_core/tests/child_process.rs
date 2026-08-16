//! Integration tests for the child-process op surface in `oam_core::child`
//! (and the unix-only `child_unix` raw registry). Covers branches added this
//! session: the stdout busy-flag that serializes concurrent reads, and the
//! liveness guards that keep a kill-after-exit from masking the real exit
//! code (or signaling a recycled pid).
//!
//! T3/T4/T5 are cfg(unix): they compile there and are skipped on Windows.
//! Every spawn goes through the functions under test with a portable shell
//! builtin (`cmd /c` / `sh -c`), never an external dependency like `node`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use oam_core::OpOutcome;
use oam_core::child::{
    ChildProcess, ChildRegistry, child_read_stdout, child_wait, spawn_child, stdio_pipe_all,
};

#[cfg(unix)]
use oam_core::child::child_kill;
#[cfg(unix)]
use oam_core::child_unix::{RawChildRegistry, StdioFd, raw_kill, raw_wait, spawn_extra};

/// Spawn a child through `spawn_child` that writes `marker` to stdout and
/// then exits. cfg-gated on the shell, not the whole test: `cmd /c echo` on
/// Windows, `sh -c 'printf ...'` on unix.
fn spawn_echoer(marker: &str) -> (tokio::process::Child, u32) {
    #[cfg(windows)]
    let (command, args) = (
        "cmd".to_string(),
        vec!["/c".to_string(), format!("echo {marker}")],
    );
    #[cfg(not(windows))]
    let (command, args) = (
        "sh".to_string(),
        vec!["-c".to_string(), format!("printf '%s' '{marker}'")],
    );
    spawn_child(command, args, None, None, false, false, stdio_pipe_all()).expect("spawn echoer")
}

/// Insert a spawned child into a fresh registry and return (registry, handle).
fn register(child: tokio::process::Child, pid: u32) -> (ChildRegistry, u64) {
    let reg: ChildRegistry = Arc::new(Mutex::new(HashMap::new()));
    let handle = 1u64;
    reg.lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(handle, ChildProcess::new(child, pid));
    (reg, handle)
}

/// Poll the registry until the child's stdout pipe has been taken out by an
/// in-flight read (`stdout.is_none()`), i.e. the busy path is genuinely
/// engaged. Bounded so a regression fails fast instead of hanging.
fn wait_until_stdout_taken(reg: &ChildRegistry, handle: u64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let taken = {
            let guard = reg.lock().unwrap_or_else(|e| e.into_inner());
            guard
                .get(&handle)
                .map(|cp| cp.stdout.is_none())
                .unwrap_or(false)
        };
        if taken {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "first read never took the stdout pipe out of the registry"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// T2: two concurrent `child_read_stdout` calls on one handle must serialize
/// over the busy flag -- the second waits for the in-flight read and then
/// sees either data or a real EOF, never a spurious early `Done` that loses
/// bytes while data is still in flight.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_stdout_reads_serialize() {
    let marker = "oam_t2_marker";
    let (child, pid) = spawn_echoer(marker);
    let (reg, handle) = register(child, pid);

    // Hold stdout open past the first read: take the child's stdin and park
    // it on a channel. Dropping it closes the pipe -> EOF -> the second read
    // resolves. This makes the overlap deterministic instead of racy.
    let stdin = {
        let mut guard = reg.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .get_mut(&handle)
            .and_then(|cp| cp.stdin.take())
            .expect("echoer spawned with piped stdin")
    };
    let (stdin_tx, stdin_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = stdin_rx.await;
        drop(stdin);
    });

    // First read on a task; wait until it has actually taken the pipe out of
    // the registry (busy=true) before firing the second, so the second is
    // guaranteed to hit the busy branch rather than winning the take itself.
    let reg1 = reg.clone();
    let first = tokio::spawn(async move { child_read_stdout(reg1, handle).await });
    wait_until_stdout_taken(&reg, handle);

    let reg2 = reg.clone();
    let second = tokio::spawn(async move { child_read_stdout(reg2, handle).await });

    // The first read is parked awaiting data (stdout open, nothing written
    // yet past spawn). Yield so any misbehaving second read gets every chance
    // to resolve early against the busy loop.
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    assert!(
        !second.is_finished(),
        "second read resolved while the first held the pipe and no data was available"
    );

    // The echoer wrote its marker at spawn time and does not read stdin, so
    // what releases the reads is EOF: close stdin (via the parked task) so
    // the shell exits and stdout reaches EOF after the data.
    let _ = stdin_tx.send(());

    let first_out = first.await.expect("first read task panicked");
    let second_out = second.await.expect("second read task panicked");

    // Collect bytes in arrival order: Bytes before Done.
    let mut collected: Vec<u8> = Vec::new();
    let mut saw_done = false;
    let mut bytes_after_done = false;
    for outcome in [first_out, second_out] {
        match outcome {
            OpOutcome::Bytes(b) => {
                if saw_done {
                    bytes_after_done = true;
                }
                collected.extend_from_slice(&b);
            }
            OpOutcome::Done => saw_done = true,
            other => panic!("unexpected read outcome: {other:?}"),
        }
    }

    let text = String::from_utf8_lossy(&collected);
    assert!(
        text.contains(marker),
        "full child output not accounted for across the two reads: {text:?}"
    );
    assert!(
        !bytes_after_done,
        "a read returned Bytes after another had already reported Done (EOF)"
    );
    assert!(saw_done, "neither read observed EOF after the child exited");

    // Reap the child so the test leaves no zombie/orphan behind.
    let _ = child_wait(reg, handle).await;
}

/// T3 (unix): a kill that lands after the child already exited must not
/// overwrite the real exit report. Covers the try_wait guard in
/// `deliver_kill` (child.rs): an already-exited child is not signaled, and
/// `child_wait` reports `{code:0, signal:null}`.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deliver_kill_does_not_signal_exited_child() {
    // Exits immediately with code 0.
    let (child, pid) = spawn_child(
        "sh".to_string(),
        vec!["-c".to_string(), "exit 0".to_string()],
        None,
        None,
        false,
        false,
        stdio_pipe_all(),
    )
    .expect("spawn immediate-exit child");
    let (reg, handle) = register(child, pid);

    // Park child_wait first: it takes the Child out of the registry entry and
    // selects on wait vs the kill notifier. Give the child time to really
    // exit and tokio's reaper a chance to observe it.
    let reg_wait = reg.clone();
    let waiter = tokio::spawn(async move { child_wait(reg_wait, handle).await });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Kill after the exit: this wakes the parked waiter, whose deliver_kill
    // must see try_wait == already-exited and NOT signal the (possibly
    // recycled) pid.
    child_kill(&reg, handle, Some("SIGTERM".to_string()));

    let outcome = waiter.await.expect("child_wait task panicked");
    let OpOutcome::Json(json) = outcome else {
        panic!("expected Json exit report, got {outcome:?}");
    };
    let report: serde_json::Value = serde_json::from_str(&json).expect("exit report is json");
    assert_eq!(
        report,
        serde_json::json!({ "code": 0, "signal": null }),
        "kill-after-exit must still report the real exit code, not a signal"
    );
}

/// T4 (unix): a kill that lands after the child already exited must not
/// disturb the real exit report. The child is a zombie here (nobody has
/// waited yet), and kill(2) accepts a zombie pid while discarding the signal
/// -- so the invariant under test is raw_wait's reporting: the real
/// ExitStatus, not the requested kill, is the source of truth.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_kill_after_exit_reports_real_exit() {
    let raw = spawn_extra(
        "sh",
        &["-c".to_string(), "exit 0".to_string()],
        None,
        None,
        false,
        &[StdioFd::Ignore, StdioFd::Ignore, StdioFd::Ignore],
    )
    .expect("spawn_extra immediate-exit child");
    let reg: RawChildRegistry = Arc::new(Mutex::new(HashMap::new()));
    let id = 1u64;
    reg.lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id, raw);

    // Nobody has waited yet, so the child is unreaped and raw_kill WILL issue
    // the kill(2). Give it time to exit first: the pid is then a zombie, the
    // kernel discards the signal, and the report below must still be the real
    // exit -- not a phantom signal death.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    raw_kill(&reg, id, Some("SIGTERM".to_string()));

    let outcome = raw_wait(reg, id).await;
    let OpOutcome::Json(json) = outcome else {
        panic!("expected Json exit report, got {outcome:?}");
    };
    let report: serde_json::Value = serde_json::from_str(&json).expect("exit report is json");
    assert_eq!(
        report,
        serde_json::json!({ "code": 0, "signal": null }),
        "an already-exited raw child must report its real exit, not the kill signal"
    );
}

/// T5 (unix): a kill issued while `raw_wait` is already in flight must be
/// delivered. This is the shape every real caller has: the JS side starts the
/// wait op the moment the child spawns, so raw_wait has take()n the `Child`
/// handle long before any kill() can arrive. Liveness must therefore come
/// from the registry's `reaped` flag, not from handle presence -- gating on
/// the handle made every live extra-fd kill a silent no-op (conformance case
/// 68's sigterm leg: the child outlived an 8s window untouched).
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_kill_during_wait_delivers_signal() {
    let raw = spawn_extra(
        "sh",
        &["-c".to_string(), "sleep 30".to_string()],
        None,
        None,
        false,
        &[StdioFd::Ignore, StdioFd::Ignore, StdioFd::Ignore],
    )
    .expect("spawn_extra sleeper child");
    let reg: RawChildRegistry = Arc::new(Mutex::new(HashMap::new()));
    let id = 1u64;
    reg.lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id, raw);

    // Start the wait FIRST and give it time to take() the Child, putting the
    // registry in the state every live kill actually sees.
    let wait = tokio::spawn(raw_wait(reg.clone(), id));
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    raw_kill(&reg, id, Some("SIGTERM".to_string()));

    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), wait)
        .await
        .expect("kill must unblock raw_wait (a no-op kill leaves the sleeper running 30s)")
        .expect("wait task join");
    let OpOutcome::Json(json) = outcome else {
        panic!("expected Json exit report, got {outcome:?}");
    };
    let report: serde_json::Value = serde_json::from_str(&json).expect("exit report is json");
    assert_eq!(
        report,
        serde_json::json!({ "code": null, "signal": "SIGTERM" }),
        "a kill during an in-flight wait must terminate the child and report the signal"
    );
}
