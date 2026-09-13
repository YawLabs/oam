//! Windows: the raw-mode switch against a REAL console.
//!
//! The pipe-based tests never reach the interesting code -- `tty_set_raw_mode`
//! returns at the `GetConsoleMode` failure long before the pending-read cancel
//! -- and the `oam_core::stdin` unit tests stop at the state machine. So these
//! tests put `oam run` behind a pseudoconsole (`CreatePseudoConsole` + an
//! `EXTENDED_STARTUPINFO_PRESENT` child, the same mechanism Windows Terminal
//! uses) and drive it through the paths that only exist on a console:
//!
//! - cooked -> raw with a read already pending: one keystroke must arrive as
//!   `data` WITHOUT Enter, and the line the cancel discarded must never
//!   surface as data;
//! - cooked -> raw on the buffer's LAST ROW: the synthetic Enter is echoed as
//!   a newline that scrolls the buffer, and the cursor restore has to put the
//!   next write right back after the text that was on screen;
//! - raw -> cooked with a raw read pending: the cancelled read must not eat
//!   the next prompt's first keystroke, which shows up as the console's echo
//!   of the answer losing it;
//! - raw -> cooked on the buffer's LAST ROW: a raw read echoes nothing, so the
//!   cancel must not move the cursor at all.
//!
//! Where the cursor ended up is judged on a replayed [`Screen`], not by
//! pattern-matching the escape stream: ConPTY is free to express one result as
//! nothing at all, a newline and a move back, a scroll, or a repaint, and what
//! a user sees is only where the next write lands.
//!
//! Not reachable from here: `EnterEcho::CarriageReturn`, a line read without
//! ENABLE_PROCESSED_INPUT. The child starts in the console's default input
//! mode, and `setRawMode` only ever clears PROCESSED_INPUT together with
//! LINE_INPUT, so that arm is covered by the `plan_console_switch` and
//! `SavedCursor` unit tests alone.
//!
//! ## Handing the child a WRITABLE console
//!
//! One flag makes this work, and leaving it out is silent. With neither handle
//! inheritance nor `STARTF_USESTDHANDLES`, CreateProcess copies this process's
//! std handles into a console child, and attaching the child to the
//! pseudoconsole replaces only the ones that are console handles
//! (microsoft/terminal#11276). So whenever cargo's own stdio is redirected --
//! a pipe under an automated runner or an agent's shell, a file under `> log`
//! -- the child keeps those handles, and everything it prints goes around the
//! pty. (Run from an interactive console tab they ARE console handles and get
//! replaced, so a run there can pass without the flag.)
//!
//! `STARTF_USESTDHANDLES` with all three handles left NULL stops the copy, and
//! the child's startup opens fresh handles on the pseudoconsole instead --
//! opened for READ AND WRITE, which is the part that matters. Both halves of
//! the cancel need a writable console input handle: `SetConsoleMode` (the flip
//! itself) and `WriteConsoleInputW` (the synthetic Enter).
//!
//! The obvious alternative does NOT work, and it fails in a way that quietly
//! makes every raw-mode assertion vacuous: routing through
//! `cmd /c <command> <CONIN$ >CONOUT$` also puts the child's stdio on the
//! console, but `<CONIN$` opens console input READ-ONLY, so `setRawMode(true)`
//! fails with ERROR_ACCESS_DENIED (measured on Windows 11 26200) and the child
//! never goes raw. YawLabs/oam#109.
//!
//! ## A test target of its own
//!
//! Rather than a module of e2e.rs: windows-sys links `CreatePseudoConsole` as a
//! static import, so on a Windows without that export (before 10 1809) the
//! test binary holding it fails to LOAD. Kept apart, that costs these tests and
//! not the whole e2e suite.

#![cfg(windows)]

use std::io::{Read, Write};
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Console::{COORD, ClosePseudoConsole, CreatePseudoConsole};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, InitializeProcThreadAttributeList,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    STARTUPINFOW, TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};

/// The pseudoconsole's size. `PTY_ROWS` is load-bearing for the last-row
/// tests: they park the cursor on the buffer's last row by writing more
/// newlines than it has rows, and [`Screen`] replays the output at this size.
const PTY_COLS: i16 = 120;
const PTY_ROWS: i16 = 40;

/// A fresh per-run temp file. The same shape as e2e.rs's helper, under its
/// own directory prefix so the two test binaries never share a run dir.
fn write_temp(name: &str, content: &str) -> PathBuf {
    static RUN_DIR: OnceLock<PathBuf> = OnceLock::new();
    let dir = RUN_DIR.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("oam-conpty-{}-{nanos}", std::process::id()))
    });
    let path = dir.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, content).unwrap();
    path
}

/// A pseudoconsole with a child attached: writes to `input` are keystrokes,
/// everything the child paints lands in `seen`.
struct ConPty {
    hpc: isize,
    input: std::fs::File,
    process: OwnedHandle,
    thread: OwnedHandle,
    attribute_list: Vec<u8>,
    seen: Arc<Mutex<Vec<u8>>>,
}

fn new_pipe() -> (HANDLE, HANDLE) {
    let mut read: HANDLE = std::ptr::null_mut();
    let mut write: HANDLE = std::ptr::null_mut();
    // SAFETY: both out-parameters are live stack handles; the null
    // security attributes and zero size are the documented defaults. The
    // return is checked before either handle is used.
    let ok = unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), 0) };
    assert!(ok != 0, "CreatePipe");
    (read, write)
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// This process's environment with `overrides` applied, in the
/// double-NUL-terminated UTF-16 form CreateProcessW wants under
/// CREATE_UNICODE_ENVIRONMENT.
fn environment_block(overrides: &[(&str, String)]) -> Vec<u16> {
    let mut block: Vec<u16> = Vec::new();
    for (key, value) in std::env::vars() {
        if overrides
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case(&key))
        {
            continue;
        }
        block.extend(wide(&format!("{key}={value}")));
    }
    for (key, value) in overrides {
        block.extend(wide(&format!("{key}={value}")));
    }
    block.push(0);
    block
}

impl ConPty {
    /// `oam run <script> --no-check` on a fresh pseudoconsole.
    fn spawn(script: &Path) -> Self {
        Self::spawn_command(&format!(
            "\"{}\" run \"{}\" --no-check",
            env!("CARGO_BIN_EXE_oam"),
            script.display()
        ))
    }

    /// Run `command_line` on a fresh pseudoconsole, with its stdio bound
    /// to that console -- see the docs at the top of this file for why that takes
    /// STARTF_USESTDHANDLES and three NULL handles rather than nothing at
    /// all.
    fn spawn_command(command_line: &str) -> Self {
        let (in_read, in_write) = new_pipe();
        let (out_read, out_write) = new_pipe();

        let mut hpc: isize = 0;
        let size = COORD {
            X: PTY_COLS,
            Y: PTY_ROWS,
        };
        // SAFETY: both handles come from CreatePipe above and are still
        // open; `hpc` is a live stack out-parameter. The HRESULT is
        // checked before the console is used.
        let hr = unsafe { CreatePseudoConsole(size, in_read, out_write, 0, &mut hpc) };
        assert_eq!(hr, 0, "CreatePseudoConsole");
        // The pseudoconsole owns its ends now; this side keeps the others.
        // SAFETY: both handles are open and owned here, and neither is
        // used again after the close.
        unsafe {
            CloseHandle(in_read);
            CloseHandle(out_write);
        }

        // Size the attribute list, then fill it with the console handle.
        let mut bytes: usize = 0;
        // SAFETY: the documented sizing call -- a null list with a live
        // out-parameter for the size. It reports failure (the buffer is
        // absent), so the return is deliberately not asserted.
        unsafe {
            InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut bytes);
        }
        assert!(bytes > 0, "attribute list size");
        let mut attribute_list = vec![0u8; bytes];
        let list = attribute_list.as_mut_ptr().cast::<core::ffi::c_void>();
        // SAFETY: `list` points at `bytes` writable bytes just allocated
        // for exactly this call, and `bytes` is the size that call asked
        // for. The list is deleted in Drop before the buffer is freed.
        let ok = unsafe { InitializeProcThreadAttributeList(list, 1, 0, &mut bytes) };
        assert!(ok != 0, "InitializeProcThreadAttributeList");
        // SAFETY: `list` is the initialised attribute list. For
        // PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE lpValue is the HPCON VALUE
        // itself, not a pointer to it (the handle is pointer-sized and
        // opaque) -- passing its address instead leaves the child with no
        // console at all, which is silent: it starts, writes into
        // nothing, and exits 0.
        let ok = unsafe {
            UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                hpc as *const core::ffi::c_void,
                std::mem::size_of::<isize>(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        assert!(ok != 0, "UpdateProcThreadAttribute");

        let mut startup = STARTUPINFOEXW::default();
        startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        startup.lpAttributeList = list;
        // The whole reason the child's output reaches the pty: see the docs
        // at the top of this file. The three handles stay NULL, so none of
        // this process's std handles are copied into the child, and its
        // startup opens read/write handles on the pseudoconsole instead.
        startup.StartupInfo.dwFlags |= STARTF_USESTDHANDLES;

        let cache = write_temp("oam-cache-conpty/.keep", "")
            .parent()
            .unwrap()
            .to_path_buf();
        let mut command = wide(command_line);
        // The child gets this process's environment with the cache
        // redirected -- built as a block rather than set with set_var,
        // which would mutate the environment of every other test sharing
        // this binary.
        let environment = environment_block(&[
            ("OAM_CACHE_DIR", cache.display().to_string()),
            ("OAM_DAEMON_IDLE_MS", "45000".to_string()),
        ]);
        let mut info = PROCESS_INFORMATION::default();
        // SAFETY: `command` is a live NUL-terminated UTF-16 buffer (the
        // API writes into it, hence `as_mut_ptr`), `startup` is a live
        // STARTUPINFOEXW whose cb and attribute list are set above, and
        // `info` is a live out-parameter. The null application name,
        // security attributes, environment and directory are the
        // documented defaults.
        let ok = unsafe {
            CreateProcessW(
                std::ptr::null(),
                command.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                environment.as_ptr().cast::<core::ffi::c_void>(),
                std::ptr::null(),
                std::ptr::addr_of!(startup).cast::<STARTUPINFOW>(),
                &mut info,
            )
        };
        assert!(ok != 0, "CreateProcessW under the pseudoconsole");

        // SAFETY: every handle below was just produced by the calls above
        // and is owned by nothing else; each is wrapped exactly once.
        let (process, thread, mut reader, input) = unsafe {
            (
                OwnedHandle::from_raw_handle(info.hProcess),
                OwnedHandle::from_raw_handle(info.hThread),
                std::fs::File::from_raw_handle(out_read),
                std::fs::File::from_raw_handle(in_write),
            )
        };
        let seen = Arc::new(Mutex::new(Vec::<u8>::new()));
        {
            let seen = Arc::clone(&seen);
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Err(e) => {
                            seen.lock()
                                .unwrap()
                                .extend_from_slice(format!("[READ ERROR {e}]").as_bytes());
                            break;
                        }
                        Ok(n) => seen.lock().unwrap().extend_from_slice(&buf[..n]),
                    }
                }
            });
        }
        Self {
            hpc,
            input,
            process,
            thread,
            attribute_list,
            seen,
        }
    }

    fn raw_output(&self) -> String {
        String::from_utf8_lossy(&self.seen.lock().unwrap()).into_owned()
    }

    /// The output with the escapes a terminal would consume stripped, so
    /// assertions read like what a person sees.
    fn text(&self) -> String {
        let raw = self.raw_output();
        let mut out = String::with_capacity(raw.len());
        let mut chars = raw.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '\u{1b}' {
                if c != '\r' {
                    out.push(c);
                }
                continue;
            }
            match chars.next() {
                // CSI: parameters, then one final byte.
                Some('[') => {
                    for p in chars.by_ref() {
                        if p.is_ascii_alphabetic() || p == '@' || p == '`' {
                            break;
                        }
                    }
                }
                // OSC: runs to BEL or ST.
                Some(']') => {
                    while let Some(p) = chars.next() {
                        if p == '\u{7}' {
                            break;
                        }
                        if p == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// Assert that `second` landed on screen immediately after `first`, the
    /// way a person would see it -- see [`written_right_after`].
    fn assert_written_right_after(&self, first: &str, second: &str) {
        if let Err(why) = written_right_after(&self.raw_output(), first, second) {
            panic!("{why}\nraw: {:?}", self.raw_output());
        }
    }

    /// The screen as it stood just before `marker` was first written.
    fn screen_before(&self, marker: &str) -> Screen {
        let raw = self.raw_output();
        let at = raw
            .find(marker)
            .unwrap_or_else(|| panic!("never saw {marker:?}: {raw:?}"));
        Screen::replay(&raw[..at])
    }

    fn wait_for(&self, needle: &str) -> String {
        self.wait_until(needle, |text| text.contains(needle))
    }

    /// Poll the escape-stripped output until `done` holds. `what` names the
    /// wait in the failure message.
    ///
    /// Gives up after 45 s, or 5 s after the child exits: a child that has
    /// exited writes nothing more, and 5 s is far past what the pseudoconsole
    /// takes to render and flush its last output. Without that, an assertion
    /// on output a finished child never printed -- the usual way these tests
    /// fail -- sat out the full 45 s.
    fn wait_until(&self, what: &str, done: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(45);
        let mut exited_at: Option<Instant> = None;
        loop {
            let text = self.text();
            if done(&text) {
                return text;
            }
            if exited_at.is_none() && self.exit_code().is_some() {
                exited_at = Some(Instant::now());
            }
            let drained = exited_at.is_some_and(|at| at.elapsed() > Duration::from_secs(5));
            assert!(
                Instant::now() < deadline && !drained,
                "never saw {what} on the console; child {}; raw {:?}; text {text:?}",
                self.exit_state(),
                self.raw_output()
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.input.write_all(bytes).unwrap();
        self.input.flush().unwrap();
    }

    fn wait_exit(&self) -> u32 {
        // SAFETY: `process` is a live process handle owned by this struct.
        let waited = unsafe { WaitForSingleObject(self.process_handle(), 30_000) };
        assert_eq!(waited, 0, "the child exits");
        let mut code: u32 = 0;
        // SAFETY: same handle, plus a live stack out-parameter; read only
        // after the call reports success.
        let ok = unsafe { GetExitCodeProcess(self.process_handle(), &mut code) };
        assert!(ok != 0, "GetExitCodeProcess");
        code
    }

    /// The child's exit code; None while it runs, or when its state cannot
    /// be read.
    fn exit_code(&self) -> Option<u32> {
        let mut code: u32 = 0;
        // SAFETY: a live process handle owned by this struct plus a live
        // stack out-parameter, read only when the call reports success.
        let ok = unsafe { GetExitCodeProcess(self.process_handle(), &mut code) };
        // STILL_ACTIVE is what a running process reports.
        (ok != 0 && code != 259).then_some(code)
    }

    /// The exit code, or that there is none yet, for a failure message.
    fn exit_state(&self) -> String {
        match self.exit_code() {
            Some(code) => format!("exited {code}"),
            None => "not exited".to_string(),
        }
    }

    fn process_handle(&self) -> HANDLE {
        use std::os::windows::io::AsRawHandle;
        self.process.as_raw_handle()
    }
}

impl Drop for ConPty {
    fn drop(&mut self) {
        // SAFETY: the child may still be running (a failed assertion skips
        // wait_exit), so it is killed before the console it is attached to
        // goes away; the attribute list is deleted while its buffer is
        // still alive. Every handle here is owned by this struct.
        unsafe {
            TerminateProcess(self.process_handle(), 1);
            ClosePseudoConsole(self.hpc);
            DeleteProcThreadAttributeList(
                self.attribute_list.as_mut_ptr().cast::<core::ffi::c_void>(),
            );
        }
        let _ = &self.thread;
    }
}

/// A minimal VT screen: enough of the escape set ConPTY emits to replay its
/// output onto a grid and say where the next write lands.
///
/// Anything outside that set PANICS rather than being guessed at, so a cursor
/// assertion made on this screen is never quietly wrong about a sequence it
/// did not understand. The exceptions set a mode or ask a question without
/// touching the grid: an allowlist of DEC private modes (see `private_mode`),
/// keyboard-protocol and device-attribute forms, and the cursor-shape DECSCUSR.
struct Screen {
    cells: Vec<Vec<char>>,
    row: usize,
    col: usize,
    /// A write into the last column leaves the cursor there with the wrap
    /// pending; the next printable wraps first (VT's deferred wrap).
    wrap_pending: bool,
    saved: (usize, usize),
}

impl Screen {
    fn new(rows: usize, cols: usize) -> Self {
        Self {
            cells: vec![vec![' '; cols]; rows],
            row: 0,
            col: 0,
            wrap_pending: false,
            saved: (0, 0),
        }
    }

    /// `output` replayed onto a blank pseudoconsole-sized screen. It must end
    /// on a sequence boundary: cut it just before a printable, never inside an
    /// escape.
    fn replay(output: &str) -> Self {
        let mut screen = Self::new(PTY_ROWS as usize, PTY_COLS as usize);
        screen.feed(output);
        screen
    }

    fn rows(&self) -> usize {
        self.cells.len()
    }

    fn cols(&self) -> usize {
        self.cells[0].len()
    }

    /// Row `row` as text, trailing blanks dropped.
    fn line(&self, row: usize) -> String {
        let line: String = self.cells[row].iter().collect();
        line.trim_end().to_string()
    }

    /// Every non-blank row, numbered from 1 the way conhost counts, for a
    /// failure message.
    fn dump(&self) -> String {
        (0..self.rows())
            .filter(|&row| !self.line(row).is_empty())
            .map(|row| format!("{:>3}|{}", row + 1, self.line(row)))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Where the next printable character goes, as (row, column).
    fn next_write(&self) -> (usize, usize) {
        if self.wrap_pending {
            ((self.row + 1).min(self.rows() - 1), 0)
        } else {
            (self.row, self.col)
        }
    }

    fn feed(&mut self, output: &str) {
        let mut chars = output.chars();
        while let Some(c) = chars.next() {
            match c {
                '\u{1b}' => match chars.next() {
                    Some('[') => {
                        let mut body = String::new();
                        let final_byte = loop {
                            match chars.next() {
                                Some(f @ '\u{40}'..='\u{7e}') => break f,
                                Some(p) => body.push(p),
                                None => panic!("the output ends inside CSI {body:?}"),
                            }
                        };
                        self.csi(&body, final_byte);
                    }
                    // OSC (the window title): runs to BEL or ST, paints nothing.
                    Some(']') => loop {
                        match chars.next() {
                            Some('\u{7}') | None => break,
                            Some('\u{1b}') => {
                                chars.next();
                                break;
                            }
                            Some(_) => {}
                        }
                    },
                    Some('7') => self.saved = (self.row, self.col),
                    Some('8') => {
                        (self.row, self.col) = self.saved;
                        self.wrap_pending = false;
                    }
                    Some('D') => self.line_feed(),
                    Some('E') => {
                        self.col = 0;
                        self.line_feed();
                    }
                    Some('M') => self.reverse_index(),
                    // Charset designation: one more byte, nothing painted.
                    Some('(' | ')' | '*' | '+') => {
                        chars.next();
                    }
                    Some('=' | '>') => {}
                    other => panic!("Screen does not model ESC {other:?}"),
                },
                '\r' => {
                    self.col = 0;
                    self.wrap_pending = false;
                }
                '\n' | '\u{b}' | '\u{c}' => self.line_feed(),
                '\u{8}' => {
                    self.col = self.col.saturating_sub(1);
                    self.wrap_pending = false;
                }
                '\t' => self.col = ((self.col / 8 + 1) * 8).min(self.cols() - 1),
                '\u{7}' => {}
                c if c.is_control() => panic!("Screen does not model control {c:?}"),
                c => self.print(c),
            }
        }
    }

    fn print(&mut self, c: char) {
        if self.wrap_pending {
            self.col = 0;
            self.line_feed();
        }
        self.cells[self.row][self.col] = c;
        if self.col + 1 == self.cols() {
            self.wrap_pending = true;
        } else {
            self.col += 1;
        }
    }

    fn line_feed(&mut self) {
        self.wrap_pending = false;
        if self.row + 1 == self.rows() {
            self.scroll_up(1);
        } else {
            self.row += 1;
        }
    }

    fn reverse_index(&mut self) {
        self.wrap_pending = false;
        if self.row == 0 {
            self.scroll_down(1);
        } else {
            self.row -= 1;
        }
    }

    fn scroll_up(&mut self, count: usize) {
        let (rows, cols) = (self.rows(), self.cols());
        for _ in 0..count.min(rows) {
            self.cells.remove(0);
            self.cells.push(vec![' '; cols]);
        }
    }

    fn scroll_down(&mut self, count: usize) {
        let (rows, cols) = (self.rows(), self.cols());
        for _ in 0..count.min(rows) {
            self.cells.pop();
            self.cells.insert(0, vec![' '; cols]);
        }
    }

    /// A DEC private sequence, `ESC[?` already taken off. Only modes that
    /// neither move the cursor nor change what is on screen or where text
    /// lands get through. The rest panic -- the alternate screen (?47, ?1047,
    /// ?1049), column mode (?3), origin mode (?6), autowrap (?7), margins
    /// (?69) and anything unlisted -- because honouring them is work this
    /// model does not do.
    fn private_mode(modes: &str, final_byte: char) {
        // Cursor keys, cursor blink, cursor visibility, focus reporting,
        // bracketed paste, synchronized output, win32-input-mode.
        const INERT: [&str; 7] = ["1", "12", "25", "1004", "2004", "2026", "9001"];
        match final_byte {
            'h' | 'l' if modes.split(';').all(|mode| INERT.contains(&mode)) => {}
            // Status and keyboard-protocol queries: they ask, and paint nothing.
            'n' | 'u' => {}
            _ => panic!("Screen does not model CSI \"?{modes}\"{final_byte}"),
        }
    }

    fn blank(&mut self, row: usize, columns: std::ops::Range<usize>) {
        for col in columns {
            self.cells[row][col] = ' ';
        }
    }

    fn csi(&mut self, body: &str, final_byte: char) {
        if let Some(modes) = body.strip_prefix('?') {
            Self::private_mode(modes, final_byte);
            return;
        }
        // Keyboard-protocol settings and device-attribute queries: they
        // configure or ask, and put nothing on the grid.
        if body.starts_with(['>', '=', '<']) {
            return;
        }
        if body.contains(|c: char| (' '..='/').contains(&c)) {
            // DECSCUSR (`ESC[2 q`), the cursor's shape, is the one
            // intermediate-byte form with a reason to be here.
            assert!(
                body.ends_with(' ') && final_byte == 'q',
                "Screen does not model CSI {body:?}{final_byte}"
            );
            return;
        }
        let params: Vec<usize> = body.split(';').map(|p| p.parse().unwrap_or(0)).collect();
        // A missing or zero count or position means 1.
        let n = |index: usize| params.get(index).copied().filter(|&v| v > 0).unwrap_or(1);
        let (rows, cols) = (self.rows(), self.cols());
        let (row, col) = (self.row, self.col);
        match final_byte {
            // CUP / HVP.
            'H' | 'f' => {
                (self.row, self.col) = ((n(0) - 1).min(rows - 1), (n(1) - 1).min(cols - 1));
            }
            // VPA, then CHA / HPA.
            'd' => self.row = (n(0) - 1).min(rows - 1),
            'G' | '`' => self.col = (n(0) - 1).min(cols - 1),
            // CUU, CUD / VPR, CUF / HPR, CUB, CNL, CPL.
            'A' => self.row = row.saturating_sub(n(0)),
            'B' | 'e' => self.row = (row + n(0)).min(rows - 1),
            'C' | 'a' => self.col = (col + n(0)).min(cols - 1),
            'D' => self.col = col.saturating_sub(n(0)),
            'E' => (self.row, self.col) = ((row + n(0)).min(rows - 1), 0),
            'F' => (self.row, self.col) = (row.saturating_sub(n(0)), 0),
            // ED / EL / ECH: erase, and the cursor stays put.
            'J' => match params[0] {
                0 => {
                    self.blank(row, col..cols);
                    (row + 1..rows).for_each(|r| self.blank(r, 0..cols));
                }
                1 => {
                    (0..row).for_each(|r| self.blank(r, 0..cols));
                    self.blank(row, 0..col + 1);
                }
                2 | 3 => (0..rows).for_each(|r| self.blank(r, 0..cols)),
                other => panic!("Screen does not model ED {other}"),
            },
            'K' => match params[0] {
                0 => self.blank(row, col..cols),
                1 => self.blank(row, 0..col + 1),
                2 => self.blank(row, 0..cols),
                other => panic!("Screen does not model EL {other}"),
            },
            'X' => self.blank(row, col..(col + n(0)).min(cols)),
            // ICH / DCH, within the row.
            '@' => (0..n(0).min(cols - col)).for_each(|_| {
                self.cells[row].pop();
                self.cells[row].insert(col, ' ');
            }),
            'P' => (0..n(0).min(cols - col)).for_each(|_| {
                self.cells[row].remove(col);
                self.cells[row].push(' ');
            }),
            // IL / DL, with the whole screen as the scroll region.
            'L' => {
                (0..n(0).min(rows - row)).for_each(|_| {
                    self.cells.pop();
                    self.cells.insert(row, vec![' '; cols]);
                });
                self.col = 0;
            }
            'M' => {
                (0..n(0).min(rows - row)).for_each(|_| {
                    self.cells.remove(row);
                    self.cells.push(vec![' '; cols]);
                });
                self.col = 0;
            }
            // SU / SD.
            'S' => self.scroll_up(n(0)),
            'T' => self.scroll_down(n(0)),
            // SCOSC / SCORC.
            's' if body.is_empty() => self.saved = (row, col),
            'u' if body.is_empty() => (self.row, self.col) = self.saved,
            // SGR, window operations, reports: nothing on the grid.
            'm' | 't' | 'n' | 'c' => return,
            other => panic!("Screen does not model CSI {body:?}{other}"),
        }
        self.wrap_pending = false;
    }
}

/// Whether, in `output`, `second` was written immediately after `first` on the
/// replayed screen: the check behind [`ConPty::assert_written_right_after`],
/// pulled out so it is testable without a console. Both markers must be free
/// of spaces, which ConPTY renders as cursor moves.
fn written_right_after(output: &str, first: &str, second: &str) -> Result<(), String> {
    let at = output
        .find(second)
        .ok_or_else(|| format!("{second:?} never appeared"))?;
    let screen = Screen::replay(&output[..at]);
    let (row, col) = screen.next_write();
    let before: String = screen.cells[row][..col].iter().collect();
    if before.ends_with(first) {
        Ok(())
    } else {
        Err(format!(
            "{second:?} was written at row {} column {}, not right after {first:?}; \
             that row reads {:?}. The screen before it:\n{}",
            row + 1,
            col + 1,
            screen.line(row),
            screen.dump()
        ))
    }
}

#[test]
fn screen_replays_text_cursor_moves_and_erases() {
    let screen =
        Screen::replay("\u{1b}[?25l\u{1b}[2J\u{1b}[m\u{1b}[Hname?\u{1b}[1Cbob\r\nX\u{1b}[3;5HY");
    assert_eq!(screen.line(0), "name? bob");
    assert_eq!(screen.line(1), "X");
    assert_eq!(screen.line(2), "    Y");
    assert_eq!(screen.next_write(), (2, 5));

    let screen = Screen::replay("abcdef\u{1b}[3D\u{1b}[K");
    assert_eq!(screen.line(0), "abc");
    assert_eq!(screen.next_write(), (0, 3));
}

#[test]
fn screen_scrolls_from_the_last_row_and_a_bare_cup_goes_home() {
    let parked = "\n".repeat(60);
    let screen = Screen::replay(&format!("{parked}[pre]\r\n"));
    assert_eq!(
        screen.line(PTY_ROWS as usize - 2),
        "[pre]",
        "the newline scrolled it up a row"
    );
    assert_eq!(screen.next_write(), (PTY_ROWS as usize - 1, 0));

    let screen = Screen::replay(&format!("{parked}[pre]\u{1b}[H"));
    assert_eq!(screen.next_write(), (0, 0));
}

#[test]
fn screen_defers_the_wrap_from_the_last_column() {
    let full = "x".repeat(PTY_COLS as usize);
    let screen = Screen::replay(&full);
    assert_eq!(screen.line(0), full);
    assert_eq!(
        screen.next_write(),
        (1, 0),
        "the wrap is pending, not taken"
    );
    assert_eq!(screen.line(1), "");

    // What tells a deferred wrap from an eager one: a CR while the wrap is
    // pending returns to the START of the full row, and the next character
    // overwrites its first cell. An eager wrap would have put it on row 2.
    let screen = Screen::replay(&format!("{full}\rY"));
    assert!(screen.line(0).starts_with("Yx"), "{:?}", screen.line(0));
    assert_eq!(screen.next_write(), (0, 1));
}

#[test]
#[should_panic(expected = "does not model CSI")]
fn screen_refuses_a_private_mode_that_changes_the_screen() {
    // The alternate screen: honouring it would mean a second grid.
    Screen::replay("\u{1b}[?25l\u{1b}[?1049h");
}

#[test]
#[should_panic(expected = "does not model CSI")]
fn screen_refuses_a_sequence_it_does_not_model() {
    // DECSTBM: a scroll region would change what a newline does.
    Screen::replay("\u{1b}[5;10r");
}

#[test]
fn written_right_after_follows_the_cursor_through_moves_and_scrolls() {
    let parked = "\n".repeat(60);
    let last = PTY_ROWS;
    // Nothing between the markers.
    assert_eq!(written_right_after("[raw][off]", "[raw]", "[off]"), Ok(()));
    // A newline, then a move straight back.
    assert_eq!(
        written_right_after("[pre]\r\n\u{1b}[1;6H[post]", "[pre]", "[post]"),
        Ok(())
    );
    // On the last row the newline scrolls, so "straight back" is a row up.
    let scrolled_back = format!("{parked}[pre]\r\n\u{1b}[{};6H[post]", last - 1);
    assert_eq!(
        written_right_after(&scrolled_back, "[pre]", "[post]"),
        Ok(())
    );
    // The same move without allowing for the scroll lands on the blank row.
    let not_stepped_up = format!("{parked}[pre]\r\n\u{1b}[{last};6H[post]");
    assert!(written_right_after(&not_stepped_up, "[pre]", "[post]").is_err());
    // No restore at all: the next line.
    let not_restored = format!("{parked}[pre]\r\n[post]");
    assert!(written_right_after(&not_restored, "[pre]", "[post]").is_err());
    // The raw -> cooked regression this file guards, as measured: the cursor
    // put back a row too high from the last row.
    let moved_up = format!(
        "{parked}[raw]\u{1b}[?25l\u{1b}[{};6H\u{1b}[?25h[off]",
        last - 1
    );
    let err = written_right_after(&moved_up, "[raw]", "[off]").unwrap_err();
    assert!(err.contains(&format!("row {}", last - 1)), "{err}");
    // A repaint that homes the cursor and redraws, then puts it back: fine.
    assert_eq!(
        written_right_after(
            "[raw]\u{1b}[H\u{1b}[2J[raw]\u{1b}[1;6H[off]",
            "[raw]",
            "[off]"
        ),
        Ok(())
    );
}

/// The harness itself: a pseudoconsole that paints nothing would make
/// every assertion below vacuous, so prove it carries output first.
#[test]
fn the_pseudoconsole_harness_carries_child_output() {
    let pty = ConPty::spawn_command("cmd.exe /c echo HARNESS_OK");
    pty.wait_for("HARNESS_OK");
    assert_eq!(pty.wait_exit(), 0);
}

/// A read is pending in COOKED mode when the TUI goes raw: the keystroke
/// must arrive without Enter, and the line the cancel discarded must not
/// be delivered as data.
#[test]
fn raw_mode_after_a_prompt_delivers_a_keystroke_without_enter() {
    let script = write_temp(
        "conpty_raw.mjs",
        r#"
import * as readline from 'node:readline/promises';
const rl = readline.createInterface({ input: process.stdin, output: process.stdout });
const answer = await rl.question('name? ');
rl.close();
console.log('[answer ' + JSON.stringify(answer) + ']');
// The Readable refills after every push, so by now a COOKED read is blocked
// in the console -- the shape a TUI started after a prompt has.
await new Promise((r) => setTimeout(r, 300));
process.stdin.setRawMode(true);
process.stdin.resume();
console.log('[raw on isRaw=' + process.stdin.isRaw + ' isTTY=' + process.stdin.isTTY + ']');
const keys = [];
process.stdin.on('data', (d) => {
  const chunk = d.toString();
  // The harness sends no CR or LF after the answer, so one arriving here can
  // only be the line the cancel was meant to discard. Say so at once, rather
  // than leaving the harness to time out on a key log that can never match.
  if (chunk.includes('\r') || chunk.includes('\n')) {
    console.log('[LEAK ' + JSON.stringify(chunk) + ']');
    process.exit(3);
  }
  keys.push(chunk);
  console.log('[keys ' + JSON.stringify(keys) + ']');
  if (keys.length === 2) process.exit(0);
});
"#,
    );
    let mut pty = ConPty::spawn(&script);
    // The console renders the prompt trailing space as a cursor move, so
    // the stripped text ends at the "?".
    pty.wait_for("name?");
    pty.send(b"bob\r");
    pty.wait_for("[answer \"bob\"]");
    // Load-bearing: if the child ever failed to go raw, every
    // assertion below would pass for the wrong reason.
    pty.wait_for("[raw on isRaw=true");

    // ONE byte, no Enter. Before the cancel landed, this produced nothing
    // until the user pressed Enter.
    pty.send(b"h");
    let text = pty.wait_until("the first key log, or a leak", |text| {
        text.contains("[keys [\"h\"]]") || text.contains("[LEAK")
    });
    assert!(
        !text.contains("[LEAK"),
        "the line the cancel discarded was delivered as data: {text:?}"
    );
    pty.send(b"i");
    pty.wait_for("[keys [\"h\",\"i\"]]");
    assert_eq!(pty.wait_exit(), 0);
}

/// Going raw with a COOKED read pending, on the buffer's LAST ROW. The
/// synthetic Enter that cancels the read is echoed as a newline, which
/// scrolls the buffer; the cursor restore has to put the next write right
/// back after the text that was on screen -- a row above where the cursor
/// was saved, since that text scrolled up with everything else.
///
/// Both halves of the restore are gated here. Skip `restore_cursor` and the
/// next write starts the fresh line under `[pre]`; skip the scroll step-up in
/// `SavedCursor::restore_target` and it lands at the right column of that
/// blank line.
#[test]
fn going_raw_on_the_last_row_puts_the_cursor_back_after_the_prompt() {
    let script = write_temp(
        "conpty_cooked_to_raw_last_row.mjs",
        r#"
import * as readline from 'node:readline/promises';
// Park the cursor on the buffer's last row: more newlines than it has rows.
process.stdout.write('\n'.repeat(60));
const rl = readline.createInterface({ input: process.stdin, output: process.stdout });
await rl.question('name? ');
rl.close();
process.stdout.write('[pre]');
// The Readable refilled after the answer, so a COOKED read is blocked in the
// console by now: the read the switch has to cancel.
await new Promise((r) => setTimeout(r, 300));
process.stdin.setRawMode(true);
process.stdout.write('[post]');
setTimeout(() => process.exit(0), 200);
"#,
    );
    let mut pty = ConPty::spawn(&script);
    pty.wait_for("name?");
    pty.send(b"bob\r");
    pty.wait_for("[post]");
    assert_eq!(pty.wait_exit(), 0);
    pty.assert_written_right_after("[pre]", "[post]");
}

/// A raw read pending across the switch back to cooked mode must not eat
/// the first keystroke of the next prompt.
///
/// The answer alone does not prove it: the bytes reach the JS stream
/// either way, so a raw read that swallows the whole of `yes\r` still
/// hands readline "yes". What a swallowed keystroke costs is the
/// CONSOLE's echo -- oam's readline does no echoing of its own, it
/// relies on ENABLE_ECHO_INPUT -- so an un-cancelled raw read shows up as
/// the answer missing from the screen. Both are asserted; the echo is the
/// one that discriminates.
#[test]
fn returning_to_cooked_mode_does_not_eat_the_next_answer() {
    let script = write_temp(
        "conpty_raw_to_cooked.mjs",
        r#"
import * as readline from 'node:readline/promises';
process.stdin.setRawMode(true);
process.stdin.resume();
const seen = [];
const onKey = (d) => seen.push(d.toString());
process.stdin.on('data', onKey);
process.stdout.write('[raw]');
// Wait for the harness's 'q'. A RAW read is pending again the moment this
// chunk is pushed -- node's Readable refills after every push -- which is
// the read the switch below has to cancel.
await new Promise((resolve) => {
  const check = () => (seen.join('').includes('q') ? resolve() : setTimeout(check, 20));
  check();
});
process.stdin.removeListener('data', onKey);
process.stdin.setRawMode(false);
const rl = readline.createInterface({ input: process.stdin, output: process.stdout });
const answer = await rl.question('again? ');
rl.close();
console.log('[again=' + JSON.stringify(answer) + ']');
process.exit(0);
"#,
    );
    let mut pty = ConPty::spawn(&script);
    pty.wait_for("[raw]");
    pty.send(b"q");
    pty.wait_for("again?");
    pty.send(b"yes\r");
    let text = pty.wait_until("the answer line", |text| {
        text.split("[again=")
            .nth(1)
            .is_some_and(|rest| rest.contains("\"]"))
    });
    assert!(
        text.contains("[again=\"yes\"]"),
        "readline must get the whole answer: {text:?}"
    );
    assert_eq!(pty.wait_exit(), 0);
    // Read the echo off the replayed screen, where an overwrite is an
    // overwrite; the escape-stripped text would read it as more text.
    let screen = pty.screen_before("[again=");
    let prompt = (0..screen.rows())
        .map(|row| screen.line(row))
        .find(|line| line.contains("again?"))
        .unwrap_or_else(|| panic!("no prompt on screen:\n{}", screen.dump()));
    assert!(
        prompt.ends_with("again? yes"),
        "the console must echo the whole answer -- a raw read left pending across \
         the switch swallows keystrokes un-echoed. The prompt row reads {prompt:?}; \
         the screen:\n{}",
        screen.dump()
    );
}

/// Leaving raw mode with a raw read pending must not move the cursor: a
/// raw read echoes nothing, so there is nothing to undo.
///
/// The cursor is parked on the buffer's LAST ROW on purpose, because that
/// is where a raw read wrongly treated as a line read gives itself away. A
/// restore saved for an Enter believed to scroll steps up a row only from the
/// last row (`SavedCursor::restore_target`); anywhere else it writes the
/// position the cursor already has, conhost paints nothing, and the
/// assertion passes whatever the code does. That is how this test read
/// before it parked the cursor here: dropping the pre-flip-mode gate in
/// `plan_console_switch`, so that every switch used `EnterEcho::Newline`,
/// left it green. (libuv only does that for a pending LINE read; a raw read
/// it cancels with a focus event and no cursor restore.)
#[test]
fn leaving_raw_mode_on_the_last_row_does_not_move_the_cursor() {
    let script = write_temp(
        "conpty_raw_off.mjs",
        r#"
// Park the cursor on the buffer's last row: more newlines than it has rows.
process.stdout.write('\n'.repeat(60));
process.stdin.setRawMode(true);
process.stdin.resume();
process.stdin.on('data', () => {});
process.stdout.write('[raw]');
// A raw read is pending by now (the stream refills after every push).
setTimeout(() => {
  process.stdin.setRawMode(false);
  process.stdout.write('[off]');
  setTimeout(() => process.exit(0), 200);
}, 500);
"#,
    );
    let pty = ConPty::spawn(&script);
    pty.wait_for("[raw]");
    pty.wait_for("[off]");
    assert_eq!(pty.wait_exit(), 0);
    pty.assert_written_right_after("[raw]", "[off]");
}
