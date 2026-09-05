//! `process.stdin`'s blocking read, and the gate that lets a console-mode
//! switch cancel one that is already in flight.
//!
//! `process.stdin` is a node Readable whose `_read` issues one [`stdin_read`]
//! op at a time, and node's Readable refills after every push: the moment a
//! line is delivered, the NEXT read is already blocked in the OS. On a Windows
//! console that read runs under the mode in force when it was issued -- with
//! `ENABLE_LINE_INPUT` set it is a cooked, line-buffered `ReadConsoleW` that
//! returns only on Enter -- and a later `SetConsoleMode` does not reach a read
//! that is already pending. So `setRawMode(true)` right after a readline
//! prompt (the shape of every TUI that follows a cooked prompt) flipped the
//! mode while a cooked read sat blocked, and everything the user typed next
//! went into the console's line buffer, invisible to the program until Enter.
//!
//! libuv has the same problem and solves it in `uv_tty_set_mode`: a pending
//! line read is cancelled by writing a synthetic VK_RETURN key event
//! (`uv__cancel_read_console`), the line that read returns is discarded, and a
//! fresh read is queued under the new mode. This module is that mechanism.
//! [`ReadGate`] tracks whether a read is blocked; the Windows raw-mode op
//! calls [`cancel_pending_console_read`] after flipping the mode, which marks
//! the read for discard and injects the Enter; the read loop drops the
//! discarded result and reads again. As in libuv, any type-ahead sitting in
//! the cooked line buffer is lost with the cancelled read, and the newline the
//! console echoed for the synthetic Enter is undone by restoring the cursor.
//!
//! Unix needs none of this: a read blocked in canonical mode picks up a
//! termios change on its own (Linux wakes the reader from `tcsetattr`; BSD
//! re-checks `ICANON` when the next byte arrives), which is also all libuv
//! does there. See the unix `tty_set_raw_mode` in oam_engine's node_ops.
//!
//! One reader is assumed: the JS Readable never has two `_read`s in flight,
//! and nothing else in the runtime reads stdin while a program runs.

use std::io::Read;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use crate::OpOutcome;

/// No read in flight.
const IDLE: u8 = 0;
/// A read is blocked in the OS.
const PENDING: u8 = 1;
/// A read is blocked in the OS and a cancel has marked its result for
/// discard; a synthetic Enter is on its way to make it return.
const DISCARD: u8 = 2;

/// How long a cancel waits for the discarded read to settle (consume the
/// injected Enter and restore the cursor). Console latency is well under a
/// millisecond; the bound only keeps a wedged console from wedging the
/// isolate thread.
const SETTLE_TIMEOUT: Duration = Duration::from_millis(250);

/// Where the cursor was when the cancel was injected. Restored once the
/// discarded read returns, undoing the newline the console echoed for the
/// synthetic Enter (libuv does the same from its read thread).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SavedCursor {
    x: i16,
    y: i16,
    /// Screen-buffer height. A cursor on the last row scrolled the buffer up
    /// when the Enter was echoed, so it goes back one row higher.
    rows: i16,
}

struct Settle {
    saved_cursor: Option<SavedCursor>,
    /// Bumped each time a discarded read settles. The cancelling thread waits
    /// for the bump so that output its caller writes right after the mode
    /// switch lands AFTER the cursor restore, never under it.
    generation: u64,
}

/// The pending-read state machine. One instance per stdin ([`STDIN_GATE`]);
/// tests build their own.
pub struct ReadGate {
    state: AtomicU8,
    settle: Mutex<Settle>,
    settled: Condvar,
}

impl Default for ReadGate {
    fn default() -> Self {
        Self::new()
    }
}

impl ReadGate {
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(IDLE),
            settle: Mutex::new(Settle {
                saved_cursor: None,
                generation: 0,
            }),
            settled: Condvar::new(),
        }
    }

    /// A read is about to block.
    fn begin(&self) {
        self.state.store(PENDING, Ordering::SeqCst);
    }

    /// The blocking read returned `bytes_read` bytes (0 for EOF or an error).
    /// `true`: the result is the caller's to deliver. `false`: a cancel marked
    /// it for discard while it was blocked; the caller drops it and reads
    /// again.
    fn end(&self, bytes_read: usize) -> bool {
        if self
            .state
            .compare_exchange(PENDING, IDLE, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return true;
        }
        self.settle_discard(bytes_read);
        self.state.store(IDLE, Ordering::SeqCst);
        false
    }

    /// Mark the read in flight, if there is one, for discard. Returns whether
    /// there was one. Pure state: the console half (the synthetic Enter that
    /// makes the marked read return) is `cancel_console_read`.
    pub fn mark_discard(&self) -> bool {
        self.state
            .compare_exchange(PENDING, DISCARD, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    /// Undo `mark_discard` when nothing will wake the read early, so it
    /// delivers its line as it would have instead of swallowing the user's
    /// next Enter. Fails (harmlessly) if the read settled in between.
    #[cfg(windows)]
    fn unmark_discard(&self) -> bool {
        self.state
            .compare_exchange(DISCARD, PENDING, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    /// Whether a read is blocked in the OS right now.
    pub fn is_pending(&self) -> bool {
        self.state.load(Ordering::SeqCst) != IDLE
    }

    fn settle_discard(&self, bytes_read: usize) {
        let mut settle = self.settle.lock().unwrap_or_else(|e| e.into_inner());
        let cursor = settle.saved_cursor.take();
        // The console only echoed a newline if the read actually returned the
        // Enter; a cancelled read that failed moved nothing.
        #[cfg(windows)]
        if let Some(cursor) = cursor
            && bytes_read > 0
        {
            console::restore_cursor(cursor);
        }
        #[cfg(not(windows))]
        let _ = (cursor, bytes_read);
        settle.generation = settle.generation.wrapping_add(1);
        self.settled.notify_all();
    }

    fn generation(&self) -> u64 {
        self.settle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .generation
    }

    /// Block until a discarded read has settled past `generation`, or
    /// `timeout` elapses. Returns whether it settled.
    fn wait_settled(&self, generation: u64, timeout: Duration) -> bool {
        let guard = self.settle.lock().unwrap_or_else(|e| e.into_inner());
        let (guard, _) = self
            .settled
            .wait_timeout_while(guard, timeout, |s| s.generation == generation)
            .unwrap_or_else(|e| e.into_inner());
        guard.generation != generation
    }
}

#[cfg(windows)]
impl ReadGate {
    /// libuv's `uv__cancel_read_console`: if a read is blocked, mark it for
    /// discard, inject a synthetic Enter so it returns, and wait for it to
    /// settle. Returns whether a read was cancelled. Call AFTER the console
    /// mode has been switched, so the read the loop re-issues runs under the
    /// new mode.
    pub fn cancel_console_read(&self) -> bool {
        if !self.mark_discard() {
            return false;
        }
        let generation = {
            let mut settle = self.settle.lock().unwrap_or_else(|e| e.into_inner());
            settle.saved_cursor = console::cursor_position();
            settle.generation
        };
        if !console::inject_enter() {
            self.unmark_discard();
            self.settle
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .saved_cursor = None;
            return false;
        }
        self.wait_settled(generation, SETTLE_TIMEOUT);
        true
    }
}

/// The gate for the process's stdin.
pub static STDIN_GATE: ReadGate = ReadGate::new();

/// Cancel the stdin read in flight, if any, after a console-mode switch. See
/// the module docs. Returns whether a read was cancelled.
#[cfg(windows)]
pub fn cancel_pending_console_read() -> bool {
    STDIN_GATE.cancel_console_read()
}

/// Read into `buf` through the gate. A result marked for discard while the
/// read was blocked is dropped and the read re-issued, so the caller only
/// ever sees a result from a read that ran under the current console mode.
/// A discarded EOF or error is dropped too -- a cancelled read says nothing
/// about the stream -- and the next read reports the real state. The gate's
/// state transitions happen on the reading thread, right around the blocking
/// call, so the window in which a cancel can catch a read that has already
/// returned is as narrow as libuv's.
pub(crate) fn read_through_gate<R: Read + ?Sized>(
    gate: &ReadGate,
    reader: &mut R,
    buf: &mut [u8],
) -> std::io::Result<usize> {
    loop {
        gate.begin();
        let result = reader.read(buf);
        let bytes_read = match &result {
            Ok(n) => *n,
            Err(_) => 0,
        };
        if gate.end(bytes_read) {
            return result;
        }
    }
}

/// The `process.stdin` read op: one blocking read of up to 64 KiB.
pub async fn stdin_read() -> OpOutcome {
    let read = tokio::task::spawn_blocking(|| {
        let mut buf = vec![0u8; 65536];
        let n = read_through_gate(&STDIN_GATE, &mut std::io::stdin().lock(), &mut buf)?;
        buf.truncate(n);
        Ok::<_, std::io::Error>(buf)
    })
    .await;
    match read {
        Ok(Ok(buf)) if buf.is_empty() => OpOutcome::Done,
        Ok(Ok(buf)) => OpOutcome::Bytes(buf),
        Ok(Err(e)) => OpOutcome::Failed(format!("stdin read: {e}")),
        Err(e) => OpOutcome::Failed(format!("stdin read: {e}")),
    }
}

/// The Win32 console half: the synthetic Enter that wakes a blocked
/// `ReadConsoleW`, and the cursor save/restore that undoes its echo. Goes
/// through `CONOUT$` rather than the stdout handle so the restore reaches the
/// console the echo went to even when stdout is redirected.
#[cfg(windows)]
mod console {
    use super::SavedCursor;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Console::{
        CONSOLE_SCREEN_BUFFER_INFO, COORD, GetConsoleScreenBufferInfo, GetStdHandle, INPUT_RECORD,
        INPUT_RECORD_0, KEY_EVENT, KEY_EVENT_RECORD, KEY_EVENT_RECORD_0, STD_INPUT_HANDLE,
        SetConsoleCursorPosition, WriteConsoleInputW,
    };

    /// VK_RETURN and its fixed set-1 scan code. A cooked read completes on
    /// the virtual key plus the character; the scan code is informational.
    const VK_RETURN: u16 = 0x0D;
    const SCAN_RETURN: u16 = 0x1C;

    fn conout_name() -> Vec<u16> {
        "CONOUT$\0".encode_utf16().collect()
    }

    /// Write one Enter key-down event to the console input queue.
    pub(super) fn inject_enter() -> bool {
        let record = INPUT_RECORD {
            EventType: KEY_EVENT as u16,
            Event: INPUT_RECORD_0 {
                KeyEvent: KEY_EVENT_RECORD {
                    bKeyDown: 1,
                    wRepeatCount: 1,
                    wVirtualKeyCode: VK_RETURN,
                    wVirtualScanCode: SCAN_RETURN,
                    uChar: KEY_EVENT_RECORD_0 {
                        UnicodeChar: u16::from(b'\r'),
                    },
                    dwControlKeyState: 0,
                },
            },
        };
        let mut written: u32 = 0;
        // SAFETY: GetStdHandle takes a documented constant by value and its
        // result is rejected below when the lookup failed. `record` is a fully
        // initialised live stack INPUT_RECORD passed by pointer with a count of
        // one, and `written` is a live stack u32 for the out-write.
        unsafe {
            let handle: HANDLE = GetStdHandle(STD_INPUT_HANDLE);
            if handle == INVALID_HANDLE_VALUE || handle.is_null() {
                return false;
            }
            WriteConsoleInputW(handle, &record, 1, &mut written) != 0 && written == 1
        }
    }

    /// The active screen buffer's cursor position, or None when there is no
    /// console to ask.
    pub(super) fn cursor_position() -> Option<SavedCursor> {
        let name = conout_name();
        let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
        // SAFETY: `name` is a NUL-terminated UTF-16 buffer that outlives the
        // call; the other CreateFileW arguments are by-value flags and the
        // null pointers it documents as optional. `info` is a live stack
        // struct for the out-write, read only after the call reports success.
        // The handle is closed on every path once the open succeeds.
        unsafe {
            let handle = CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            );
            if handle == INVALID_HANDLE_VALUE {
                return None;
            }
            let ok = GetConsoleScreenBufferInfo(handle, &mut info) != 0;
            CloseHandle(handle);
            ok.then(|| SavedCursor {
                x: info.dwCursorPosition.X,
                y: info.dwCursorPosition.Y,
                rows: info.dwSize.Y,
            })
        }
    }

    /// Put the cursor back where `cursor_position` found it, one row higher
    /// when the echoed Enter scrolled the buffer (libuv's adjustment).
    pub(super) fn restore_cursor(cursor: SavedCursor) {
        let name = conout_name();
        let mut pos = COORD {
            X: cursor.x,
            Y: cursor.y,
        };
        if cursor.y == cursor.rows - 1 && pos.Y > 0 {
            pos.Y -= 1;
        }
        // SAFETY: `name` is a NUL-terminated UTF-16 buffer that outlives the
        // call; the other CreateFileW arguments are by-value flags and the
        // null pointers it documents as optional. SetConsoleCursorPosition
        // takes the handle and a by-value COORD. The handle is closed once the
        // open succeeds.
        unsafe {
            let handle = CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            );
            if handle == INVALID_HANDLE_VALUE {
                return;
            }
            SetConsoleCursorPosition(handle, pos);
            CloseHandle(handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    /// A reader scripted with one result per read. A step can flip the gate
    /// to DISCARD before returning, standing in for a mode switch (and its
    /// synthetic Enter) that landed while the read was blocked.
    struct Scripted<'a> {
        gate: &'a ReadGate,
        steps: Vec<(bool, io::Result<&'static [u8]>)>,
        reads: usize,
    }

    impl Read for Scripted<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            assert!(self.gate.is_pending(), "read issued outside the gate");
            let (cancel, result) = self.steps.remove(0);
            self.reads += 1;
            if cancel {
                assert!(
                    self.gate.mark_discard(),
                    "cancel must find the read pending"
                );
            }
            let bytes = result?;
            buf[..bytes.len()].copy_from_slice(bytes);
            Ok(bytes.len())
        }
    }

    fn scripted<'a>(
        gate: &'a ReadGate,
        steps: Vec<(bool, io::Result<&'static [u8]>)>,
    ) -> Scripted<'a> {
        Scripted {
            gate,
            steps,
            reads: 0,
        }
    }

    #[test]
    fn uncancelled_read_is_delivered_as_is() {
        let gate = ReadGate::new();
        let mut reader = scripted(&gate, vec![(false, Ok(b"abc"))]);
        let mut buf = [0u8; 8];
        let n = read_through_gate(&gate, &mut reader, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"abc");
        assert_eq!(reader.reads, 1);
        assert!(!gate.is_pending());
        assert_eq!(gate.generation(), 0, "nothing was discarded");
    }

    #[test]
    fn cancelled_read_is_discarded_and_reissued() {
        let gate = ReadGate::new();
        // The cooked read returns the synthetic Enter; the fresh read under
        // the new mode returns the keypress.
        let mut reader = scripted(&gate, vec![(true, Ok(b"\r\n")), (false, Ok(b"h"))]);
        let mut buf = [0u8; 8];
        let n = read_through_gate(&gate, &mut reader, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"h", "the discarded line must not surface");
        assert_eq!(reader.reads, 2);
        assert!(!gate.is_pending());
        assert_eq!(gate.generation(), 1, "the discard settled once");
    }

    #[test]
    fn cancelled_eof_is_not_delivered_as_eof() {
        let gate = ReadGate::new();
        let mut reader = scripted(&gate, vec![(true, Ok(b"")), (false, Ok(b"x"))]);
        let mut buf = [0u8; 8];
        let n = read_through_gate(&gate, &mut reader, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"x");
        assert_eq!(reader.reads, 2);
    }

    #[test]
    fn cancelled_error_is_dropped_and_the_next_result_delivered() {
        let gate = ReadGate::new();
        let mut reader = scripted(
            &gate,
            vec![
                (
                    true,
                    Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled")),
                ),
                (
                    false,
                    Err(io::Error::new(io::ErrorKind::BrokenPipe, "gone")),
                ),
            ],
        );
        let mut buf = [0u8; 8];
        let err = read_through_gate(&gate, &mut reader, &mut buf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(reader.reads, 2, "one retry, then the real error, no loop");
        assert!(!gate.is_pending());
    }

    #[test]
    fn mark_discard_needs_a_pending_read() {
        let gate = ReadGate::new();
        assert!(!gate.mark_discard(), "nothing in flight to cancel");
        assert!(!gate.is_pending());
        let mut reader = scripted(&gate, vec![(false, Ok(b"a"))]);
        let mut buf = [0u8; 8];
        let n = read_through_gate(&gate, &mut reader, &mut buf).unwrap();
        assert_eq!(
            &buf[..n],
            b"a",
            "a stray cancel must not poison the next read"
        );
        assert!(!gate.mark_discard(), "the read settled; nothing to cancel");
    }

    #[test]
    fn wait_settled_returns_once_the_discard_settles() {
        let gate = ReadGate::new();
        let generation = gate.generation();
        assert!(
            !gate.wait_settled(generation, Duration::from_millis(20)),
            "nothing settles on its own"
        );
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(30));
                gate.settle_discard(2);
            });
            assert!(gate.wait_settled(generation, Duration::from_secs(5)));
        });
        assert_eq!(gate.generation(), generation + 1);
    }
}
