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
//! fresh read is queued under the new mode. This module is that mechanism, in
//! libuv's order. [`ReadGate`] tracks whether a read is blocked. The Windows
//! raw-mode op takes a `ReadGate::hold`, so the reader cannot issue its next
//! read; calls [`cancel_pending_console_read`] BEFORE it flips the mode, which
//! marks the read for discard and injects the Enter while the mode the read
//! was issued under is still in force; flips the mode; and releases the hold,
//! so the re-issued read starts under the new mode. As in libuv, any
//! type-ahead sitting in the cooked line buffer is lost with the cancelled
//! read, and whatever the console wrote for the synthetic Enter is undone by
//! restoring the cursor.
//!
//! The order is load-bearing. Injected AFTER the flip, the Enter is handled
//! under the new mode, and conhost 10.0.26100.1 (measured) answers a raw
//! mode's Enter with a bare carriage return rather than an echoed newline --
//! so the last-row adjustment in `SavedCursor::restore_target`, which assumes
//! a newline scrolled the buffer, put the cursor a row too high whenever the
//! prompt sat on the buffer's last row. Before the flip, a read already
//! blocked in ReadConsoleW handles the Enter under its own mode. (A read
//! marked PENDING but not yet inside ReadConsoleW when the settle wait runs
//! out would take the queued Enter after the flip; the wait is 250 ms against
//! a console's sub-millisecond latency.)
//!
//! The cancel runs in BOTH directions, because a read issued raw keeps raw
//! semantics across the switch back just the same, and would deliver the
//! first keystroke of the next cooked prompt immediately and un-echoed. What
//! the synthetic Enter writes is decided by the mode that read runs under
//! ([`EnterEcho`]): on conhost's rewritten cooked read, a line read writes a
//! newline whether or not it echoes -- CRLF with ENABLE_PROCESSED_INPUT, which
//! scrolls the buffer from its last row, and a bare CR without -- while a raw
//! read writes nothing. The raw-mode op passes that along: the cursor is saved
//! and put back for a line read, and stepped up a row only when a newline
//! scrolled it. (libuv steps up unconditionally, which holds only because its
//! own NORMAL mode always carries PROCESSED_INPUT.)
//!
//! The mapping is the rewritten cooked read's, the code conhost 10.0.26100
//! runs. The code before the rewrite (the terminal repo's release-1.18)
//! wrote nothing at all for a line read without ENABLE_ECHO_INPUT, so on a
//! host still running it, a console handed to oam with echo off can have its
//! cursor put back one row too high from the buffer's last row. That is read
//! off the source, not measured: only 10.0.26100.1 was at hand.
//!
//! Unix needs none of this: a read blocked in canonical mode picks up a
//! termios change on its own -- Linux and XNU both wake the reader from the
//! `tcsetattr` that clears `ICANON`, and hand it the input already pending (a
//! `TCSAFLUSH` discards that input first) -- which is also all libuv does
//! there. Both wakes are read off the kernel source; what is measured, on
//! Linux 6.6 and by a pty test that also runs on macOS, is only that a key
//! typed after the switch reaches the waiting read without Enter. The unix
//! `tty_set_raw_mode` in oam_engine's node_ops carries the detail.
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
/// isolate thread. (The cancel side -- this, `arm_cancel`, `wait_settled`,
/// `disarm_cancel` -- is driven by the Windows console half; on unix nothing
/// cancels, so those are dead there while the tests still cover them.)
#[cfg_attr(not(windows), allow(dead_code))]
const SETTLE_TIMEOUT: Duration = Duration::from_millis(250);

/// What the synthetic Enter writes to the screen when it lands on the read in
/// flight -- decided by the mode that read runs under, the PRE-flip one. From
/// conhost's rewritten cooked read (readDataCooked.cpp), and measured on
/// conhost 10.0.26100.1: a line (ENABLE_LINE_INPUT) read writes a newline
/// whether or not it echoes the typed characters, CRLF with
/// ENABLE_PROCESSED_INPUT and a bare CR without; a raw read returns the Enter
/// as one silent byte. (The pre-rewrite code wrote nothing for a line read
/// without ENABLE_ECHO_INPUT -- see the module docs.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnterEcho {
    /// A raw read: nothing on screen moves.
    Nothing,
    /// A line read without ENABLE_PROCESSED_INPUT: a bare CR.
    CarriageReturn,
    /// A line read with ENABLE_PROCESSED_INPUT: CRLF, which scrolls the buffer
    /// when the cursor sits on its last row.
    Newline,
}

/// Where the cursor was when the cancel was injected. Restored once the
/// discarded read returns, undoing what the console wrote for the synthetic
/// Enter (libuv does the same from its read thread). Only constructed by the
/// Windows console half (and the tests); the state machine around it is
/// shared so the decisions are testable everywhere.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SavedCursor {
    x: i16,
    y: i16,
    /// Screen-buffer height.
    rows: i16,
    /// Whether the Enter writes a newline, which scrolls the buffer when the
    /// cursor sits on its last row -- rather than a bare CR, which does not.
    scrolls: bool,
}

impl SavedCursor {
    /// Where the cursor goes back to: one row higher when it sat on the
    /// buffer's last row AND the Enter's newline scrolled the buffer up
    /// (libuv's adjustment in uv_tty_line_read_thread). A bare CR scrolls
    /// nothing, so the row stays. Never above row 0.
    #[cfg_attr(not(windows), allow(dead_code))]
    fn restore_target(self) -> (i16, i16) {
        let mut y = self.y;
        if self.scrolls && self.y == self.rows - 1 && y > 0 {
            y -= 1;
        }
        (self.x, y)
    }
}

struct Settle {
    /// Set by `arm_cancel` for a line read, whose injected Enter writes to the
    /// screen; None for a raw one, which writes nothing.
    saved_cursor: Option<SavedCursor>,
    /// Bumped each time a discarded read settles. The cancelling thread waits
    /// for the bump so that output its caller writes right after the mode
    /// switch lands AFTER the cursor restore, never under it.
    generation: u64,
    /// Outstanding `ReadGate::hold`s. While non-zero the reader parks in
    /// `begin` instead of issuing its next read.
    held: u32,
}

/// The pending-read state machine. One instance per stdin ([`STDIN_GATE`]);
/// tests build their own.
pub struct ReadGate {
    state: AtomicU8,
    settle: Mutex<Settle>,
    settled: Condvar,
    /// Signalled when the last hold is released.
    released: Condvar,
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
                held: 0,
            }),
            settled: Condvar::new(),
            released: Condvar::new(),
        }
    }

    /// A read is about to block. Parks first while a hold is out, and marks
    /// the read PENDING under the same lock `hold` takes -- so once `hold`
    /// returns, every read is either already PENDING (and a cancel will find
    /// it) or waiting here for the release.
    fn begin(&self) {
        let settle = self.settle.lock().unwrap_or_else(|e| e.into_inner());
        let _settle = self
            .released
            .wait_while(settle, |s| s.held > 0)
            .unwrap_or_else(|e| e.into_inner());
        self.state.store(PENDING, Ordering::SeqCst);
    }

    /// Keep the reader from issuing its next read until the returned guard
    /// drops. A read already blocked is unaffected -- cancel it separately --
    /// and one about to begin waits in `begin`. This is what lets a console
    /// mode switch cancel the pending read under the OLD mode and still have
    /// the re-issued read start under the NEW one; libuv gets the same by
    /// stopping the read before its SetConsoleMode and restarting it after.
    pub fn hold(&self) -> ReadHold<'_> {
        self.settle.lock().unwrap_or_else(|e| e.into_inner()).held += 1;
        ReadHold { gate: self }
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
    #[cfg_attr(not(windows), allow(dead_code))]
    fn unmark_discard(&self) -> bool {
        self.state
            .compare_exchange(DISCARD, PENDING, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    /// Arm a cancel: mark the read in flight for discard and remember where
    /// the cursor is, so whatever the console writes for the synthetic Enter
    /// can be undone. `cursor` is consulted only once a read is
    /// actually marked, and returns None for a raw read (nothing is written).
    /// Returns the settle generation to wait on, or None when nothing was
    /// pending.
    #[cfg_attr(not(windows), allow(dead_code))]
    fn arm_cancel(&self, cursor: impl FnOnce() -> Option<SavedCursor>) -> Option<u64> {
        if !self.mark_discard() {
            return None;
        }
        let mut settle = self.settle.lock().unwrap_or_else(|e| e.into_inner());
        settle.saved_cursor = cursor();
        Some(settle.generation)
    }

    /// Undo `arm_cancel` when the wake-up could not be sent: the read stays
    /// live and delivers as it would have, and no cursor is put back.
    #[cfg_attr(not(windows), allow(dead_code))]
    fn disarm_cancel(&self) {
        self.unmark_discard();
        self.settle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .saved_cursor = None;
    }

    /// Whether a read is blocked in the OS right now.
    pub fn is_pending(&self) -> bool {
        self.state.load(Ordering::SeqCst) != IDLE
    }

    /// A marked read returned `bytes_read` bytes. Puts the cursor back when
    /// there is one to put back, bumps the settle generation, and wakes the
    /// cancelling thread. Returns the cursor that was restored: only a line
    /// read (the one `arm_cancel` saved a cursor for) that actually returned
    /// its Enter wrote anything; a raw read's injected `\r` and a cancelled
    /// read that failed moved nothing. The decision is platform-independent;
    /// the SetConsoleCursorPosition behind it is not.
    fn settle_discard(&self, bytes_read: usize) -> Option<SavedCursor> {
        let mut settle = self.settle.lock().unwrap_or_else(|e| e.into_inner());
        let restored = settle.saved_cursor.take().filter(|_| bytes_read > 0);
        #[cfg(windows)]
        if let Some(cursor) = restored {
            console::restore_cursor(cursor);
        }
        settle.generation = settle.generation.wrapping_add(1);
        self.settled.notify_all();
        restored
    }

    /// Test-only: the cancel path reads the counter under its own lock.
    #[cfg(test)]
    fn generation(&self) -> u64 {
        self.settle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .generation
    }

    /// Block until a discarded read has settled past `generation`, or
    /// `timeout` elapses. Returns whether it settled.
    #[cfg_attr(not(windows), allow(dead_code))]
    fn wait_settled(&self, generation: u64, timeout: Duration) -> bool {
        let guard = self.settle.lock().unwrap_or_else(|e| e.into_inner());
        let (guard, _) = self
            .settled
            .wait_timeout_while(guard, timeout, |s| s.generation == generation)
            .unwrap_or_else(|e| e.into_inner());
        guard.generation != generation
    }
}

/// A [`ReadGate::hold`]. The reader resumes when the last one drops.
#[must_use = "the hold is released when the guard drops"]
pub struct ReadHold<'a> {
    gate: &'a ReadGate,
}

impl Drop for ReadHold<'_> {
    fn drop(&mut self) {
        let mut settle = self.gate.settle.lock().unwrap_or_else(|e| e.into_inner());
        settle.held -= 1;
        if settle.held == 0 {
            self.gate.released.notify_all();
        }
    }
}

#[cfg(windows)]
impl ReadGate {
    /// libuv's `uv__cancel_read_console`: if a read is blocked, mark it for
    /// discard, inject a synthetic Enter so it returns, and wait for it to
    /// settle. Returns whether a read was cancelled. Call BEFORE the console
    /// mode is switched, with a hold out: the Enter has to land while the
    /// read's own mode is still in force, and the hold keeps the re-issued
    /// read from starting until the new mode is. `echo` is what that Enter
    /// writes (see `EnterEcho`): nothing for a raw read, which leaves no
    /// cursor to restore.
    pub fn cancel_console_read(&self, echo: EnterEcho) -> bool {
        let cursor = || match echo {
            EnterEcho::Nothing => None,
            EnterEcho::CarriageReturn => console::cursor_position(false),
            EnterEcho::Newline => console::cursor_position(true),
        };
        let Some(generation) = self.arm_cancel(cursor) else {
            return false;
        };
        if !console::inject_enter() {
            self.disarm_cancel();
            return false;
        }
        self.wait_settled(generation, SETTLE_TIMEOUT);
        true
    }
}

/// The gate for the process's stdin.
pub static STDIN_GATE: ReadGate = ReadGate::new();

/// Cancel the stdin read in flight, if any, ahead of a console-mode switch,
/// while holding [`hold_console_reads`]. See the module docs. `echo`: what the
/// synthetic Enter writes under the mode the read was issued with, which
/// decides whether there is a cursor to put back and whether a scroll moved
/// it. Returns whether a read was cancelled.
#[cfg(windows)]
pub fn cancel_pending_console_read(echo: EnterEcho) -> bool {
    STDIN_GATE.cancel_console_read(echo)
}

/// Hold the stdin reader across a console-mode switch: its next read waits
/// until the guard drops. See `ReadGate::hold`.
#[cfg(windows)]
pub fn hold_console_reads() -> ReadHold<'static> {
    STDIN_GATE.hold()
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

/// What fd 0 is, in the four kinds libuv's `uv_guess_handle` sorts a handle
/// into. node's `getStdin` (lib/internal/bootstrap/switches/is_main_thread.js)
/// picks `process.stdin`'s class by it, and the class decides what EOF does:
/// a `net.Socket` or `tty.ReadStream` destroys itself after 'end', so 'close'
/// follows, while a file is an `fs.ReadStream` opened with `autoClose: false`
/// and emits 'end' alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandleType {
    /// A terminal (node: `tty.ReadStream`).
    Tty,
    /// A regular file, or a character device that is not a terminal --
    /// `< input.txt`, `/dev/null`, Windows' `NUL` (node: `fs.ReadStream`).
    File,
    /// A pipe or a socket (node: `net.Socket`).
    Pipe,
    /// Anything else, including no stdin at all (node: an empty Readable,
    /// already ended).
    Unknown,
}

impl HandleType {
    /// libuv's spelling, as node's `guessHandleType` returns it.
    pub fn as_str(self) -> &'static str {
        match self {
            HandleType::Tty => "TTY",
            HandleType::File => "FILE",
            HandleType::Pipe => "PIPE",
            HandleType::Unknown => "UNKNOWN",
        }
    }
}

/// [`HandleType`] of this process's stdin.
pub fn stdin_handle_type() -> HandleType {
    guess_handle(&std::io::stdin())
}

/// `uv_guess_handle` (src/win/handle.c): a character device is a TTY when it
/// is a console and a FILE otherwise, a pipe is a PIPE, a disk file a FILE,
/// and anything GetFileType cannot name is UNKNOWN. The terminal test is
/// `IsTerminal`, the one `process.stdin.isTTY` already reports, so the two
/// can never disagree.
#[cfg(windows)]
fn guess_handle<H>(handle: &H) -> HandleType
where
    H: std::os::windows::io::AsRawHandle + std::io::IsTerminal,
{
    use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_CHAR, FILE_TYPE_DISK, FILE_TYPE_PIPE};
    if handle.is_terminal() {
        return HandleType::Tty;
    }
    match crate::child_win::file_type(handle.as_raw_handle()) {
        FILE_TYPE_DISK | FILE_TYPE_CHAR => HandleType::File,
        FILE_TYPE_PIPE => HandleType::Pipe,
        _ => HandleType::Unknown,
    }
}

/// `uv_guess_handle` (src/unix/core.c): a terminal is a TTY, a regular file
/// or character device a FILE, a FIFO a PIPE. libuv tells a socket's family
/// apart (TCP, UDP, a unix-domain PIPE); node gives TCP and PIPE the same
/// `net.Socket`, and a datagram socket as stdin is not a shape anything
/// spawns, so every socket is a PIPE here. An fd `fstat` fails on (closed)
/// is UNKNOWN, as are directories and block devices.
#[cfg(unix)]
fn guess_handle<H>(handle: &H) -> HandleType
where
    H: std::os::fd::AsFd + std::io::IsTerminal,
{
    use rustix::fs::FileType;
    if handle.is_terminal() {
        return HandleType::Tty;
    }
    match rustix::fs::fstat(handle.as_fd()) {
        Ok(stat) => match FileType::from_raw_mode(stat.st_mode) {
            FileType::RegularFile | FileType::CharacterDevice => HandleType::File,
            FileType::Fifo | FileType::Socket => HandleType::Pipe,
            _ => HandleType::Unknown,
        },
        Err(_) => HandleType::Unknown,
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
    pub(super) fn cursor_position(scrolls: bool) -> Option<SavedCursor> {
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
            ok.then_some(SavedCursor {
                x: info.dwCursorPosition.X,
                y: info.dwCursorPosition.Y,
                rows: info.dwSize.Y,
                scrolls,
            })
        }
    }

    /// Put the cursor back where `cursor_position` found it, one row higher
    /// when the Enter's newline scrolled the buffer (see
    /// `SavedCursor::restore_target`).
    pub(super) fn restore_cursor(cursor: SavedCursor) {
        let name = conout_name();
        let (x, y) = cursor.restore_target();
        let pos = COORD { X: x, Y: y };
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

    const CURSOR: SavedCursor = SavedCursor {
        x: 7,
        y: 3,
        rows: 40,
        scrolls: true,
    };

    #[test]
    fn cooked_cancel_restores_the_cursor_once_the_enter_lands() {
        // cooked -> raw: the pending read is a cooked, echoing one; its
        // injected Enter echoes "\r\n", so the cursor goes back.
        let gate = ReadGate::new();
        gate.begin();
        assert_eq!(gate.arm_cancel(|| Some(CURSOR)), Some(0));
        assert_eq!(gate.settle_discard(2), Some(CURSOR));
        assert_eq!(gate.generation(), 1);
    }

    #[test]
    fn raw_cancel_leaves_the_cursor_alone() {
        // raw -> cooked: the pending read is raw; the injected Enter comes
        // back as one silent byte and nothing on screen moved.
        let gate = ReadGate::new();
        gate.begin();
        assert_eq!(gate.arm_cancel(|| None), Some(0));
        assert_eq!(gate.settle_discard(1), None);
        assert_eq!(gate.generation(), 1, "the discard still settled");
    }

    #[test]
    fn cooked_cancel_whose_read_returned_nothing_restores_nothing() {
        let gate = ReadGate::new();
        gate.begin();
        assert!(gate.arm_cancel(|| Some(CURSOR)).is_some());
        assert_eq!(gate.settle_discard(0), None, "no Enter came back, no echo");
    }

    #[test]
    fn arm_cancel_without_a_pending_read_is_a_noop() {
        let gate = ReadGate::new();
        assert_eq!(
            gate.arm_cancel(|| unreachable!("cursor is read only for a marked read")),
            None
        );
        let mut reader = scripted(&gate, vec![(false, Ok(b"ok"))]);
        let mut buf = [0u8; 8];
        let n = read_through_gate(&gate, &mut reader, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"ok");
        assert_eq!(gate.generation(), 0);
    }

    #[test]
    fn second_cancel_before_the_first_settles_is_a_noop() {
        // DISCARD -> DISCARD: a switch back before the first discarded read
        // has returned. The read is already marked, so the second cancel
        // must not re-mark it, must not even read its cursor, and must leave
        // the first cancel's cursor for the settle -- which happens once.
        let gate = ReadGate::new();
        gate.begin();
        assert_eq!(gate.arm_cancel(|| Some(CURSOR)), Some(0));
        assert_eq!(
            gate.arm_cancel(|| unreachable!("a second cancel must not consult its cursor")),
            None
        );
        assert!(gate.is_pending(), "still marked, still in flight");
        assert_eq!(
            gate.settle_discard(2),
            Some(CURSOR),
            "the first cancel's cursor survives the second"
        );
        assert_eq!(gate.generation(), 1, "settled exactly once");
        assert!(
            !gate.mark_discard(),
            "nothing left to cancel after the settle"
        );
    }

    #[test]
    fn disarm_cancel_lets_the_read_deliver_and_forgets_the_cursor() {
        let gate = ReadGate::new();
        gate.begin();
        assert!(gate.arm_cancel(|| Some(CURSOR)).is_some());
        gate.disarm_cancel();
        assert!(gate.end(4), "the read is live again and delivers");
        assert_eq!(gate.generation(), 0, "nothing was discarded");
        gate.begin();
        assert!(gate.arm_cancel(|| None).is_some());
        assert_eq!(gate.settle_discard(2), None, "the disarmed cursor is gone");
    }

    #[test]
    fn restore_target_steps_up_one_row_only_from_the_last_row() {
        assert_eq!(CURSOR.restore_target(), (7, 3));
        let last_row = SavedCursor {
            x: 7,
            y: 39,
            rows: 40,
            scrolls: true,
        };
        assert_eq!(last_row.restore_target(), (7, 38), "the echo scrolled");
        let bare_cr = SavedCursor {
            scrolls: false,
            ..last_row
        };
        assert_eq!(
            bare_cr.restore_target(),
            (7, 39),
            "a bare CR scrolls nothing, so the row stays"
        );
        let one_row = SavedCursor {
            x: 0,
            y: 0,
            rows: 1,
            scrolls: true,
        };
        assert_eq!(one_row.restore_target(), (0, 0), "never above row 0");
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

    /// A reader that announces each read the loop issues, then returns what
    /// the test hands it -- so a test can see whether, and when, a read was
    /// issued. `cancel` stands in for a mode switch's synthetic Enter landing
    /// while the read is blocked.
    struct Announcing<'a> {
        gate: &'a ReadGate,
        issued: std::sync::mpsc::Sender<()>,
        results: std::sync::mpsc::Receiver<(bool, &'static [u8])>,
    }

    impl Read for Announcing<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            assert!(self.gate.is_pending(), "read issued outside the gate");
            self.issued.send(()).unwrap();
            // Bounded, so a regression FAILS rather than hangs: a reader
            // blocked here forever would hold up thread::scope after the
            // assertion that caught the regression had already panicked. (A
            // reader parked in `begin` by a hold that is never released is not
            // covered -- that wait is untimed, so a broken release still hangs.)
            let (cancel, bytes) = self
                .results
                .recv_timeout(Duration::from_secs(5))
                .expect("the test never answered this read");
            if cancel {
                assert!(
                    self.gate.mark_discard(),
                    "cancel must find the read pending"
                );
            }
            buf[..bytes.len()].copy_from_slice(bytes);
            Ok(bytes.len())
        }
    }

    #[test]
    fn a_hold_parks_a_read_that_has_not_begun_until_it_is_released() {
        let gate = ReadGate::new();
        let g = &gate;
        let (issued_tx, issued) = std::sync::mpsc::channel();
        let (results_tx, results) = std::sync::mpsc::channel();
        let hold = g.hold();
        std::thread::scope(|scope| {
            let reader = scope.spawn(move || {
                let mut r = Announcing {
                    gate: g,
                    issued: issued_tx,
                    results,
                };
                let mut buf = [0u8; 8];
                let n = read_through_gate(g, &mut r, &mut buf).unwrap();
                buf[..n].to_vec()
            });
            assert!(
                issued.recv_timeout(Duration::from_millis(100)).is_err(),
                "no read may be issued while the gate is held"
            );
            drop(hold);
            issued
                .recv_timeout(Duration::from_secs(5))
                .expect("the release lets the read begin");
            results_tx.send((false, b"k")).unwrap();
            assert_eq!(reader.join().unwrap(), b"k");
        });
    }

    #[test]
    fn a_read_discarded_under_a_hold_is_reissued_only_after_the_release() {
        // The mode switch's order: hold, cancel the read in flight, switch,
        // release. The discarded read must not be re-issued in between: that
        // re-issue is the read that has to start under the NEW mode.
        let gate = ReadGate::new();
        let g = &gate;
        let (issued_tx, issued) = std::sync::mpsc::channel();
        let (results_tx, results) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let reader = scope.spawn(move || {
                let mut r = Announcing {
                    gate: g,
                    issued: issued_tx,
                    results,
                };
                let mut buf = [0u8; 8];
                let n = read_through_gate(g, &mut r, &mut buf).unwrap();
                buf[..n].to_vec()
            });
            issued
                .recv_timeout(Duration::from_secs(5))
                .expect("the first read begins");
            let hold = g.hold();
            // The cancel lands while that read is blocked: it returns the
            // synthetic Enter and is discarded.
            results_tx.send((true, b"\r\n")).unwrap();
            assert!(
                g.wait_settled(0, Duration::from_secs(5)),
                "the discard settles under the hold"
            );
            assert!(
                issued.recv_timeout(Duration::from_millis(100)).is_err(),
                "the discarded read must not be re-issued while held"
            );
            drop(hold);
            issued
                .recv_timeout(Duration::from_secs(5))
                .expect("the release re-issues the read");
            results_tx.send((false, b"h")).unwrap();
            assert_eq!(
                reader.join().unwrap(),
                b"h",
                "the discarded line never surfaces"
            );
        });
    }

    /// The classification node's stdin class hangs on, checked against real
    /// objects of each kind rather than the process's own stdin (which is
    /// whatever the test runner attached).
    #[test]
    fn guess_handle_sorts_files_devices_and_pipes() {
        let path = std::env::temp_dir().join(format!(
            "oam-guess-handle-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&path, b"x").unwrap();
        let file = std::fs::File::open(&path).unwrap();
        assert_eq!(guess_handle(&file), HandleType::File, "a disk file");
        drop(file);
        let _ = std::fs::remove_file(&path);

        // What `stdio: 'ignore'` hands a child: NUL / /dev/null, a character
        // device that is not a terminal, which libuv calls a FILE too.
        let null = std::fs::File::open(if cfg!(windows) { "NUL" } else { "/dev/null" }).unwrap();
        assert_eq!(guess_handle(&null), HandleType::File, "the null device");

        let (reader, _writer) = std::io::pipe().unwrap();
        #[cfg(windows)]
        let reader = std::os::windows::io::OwnedHandle::from(reader);
        #[cfg(unix)]
        let reader = std::os::fd::OwnedFd::from(reader);
        assert_eq!(guess_handle(&reader), HandleType::Pipe, "a pipe");
    }

    #[cfg(unix)]
    #[test]
    fn guess_handle_calls_a_directory_unknown() {
        let dir = std::fs::File::open(std::env::temp_dir()).unwrap();
        assert_eq!(guess_handle(&dir), HandleType::Unknown);
    }

    #[test]
    fn handle_type_spells_libuv_names() {
        assert_eq!(
            [
                HandleType::Tty,
                HandleType::File,
                HandleType::Pipe,
                HandleType::Unknown
            ]
            .map(HandleType::as_str),
            ["TTY", "FILE", "PIPE", "UNKNOWN"]
        );
    }
}
