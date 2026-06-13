//! V8 Inspector (Chrome DevTools Protocol) integration.
//!
//! The v8-free WebSocket transport lives in `oam_core::inspector`; this
//! module owns the V8 side: the `V8Inspector`, the session, and the client
//! and channel callbacks that bridge V8 <-> transport.
//!
//! Threading: V8 and the session are `!Send` and stay on the isolate thread.
//! The transport runs on its own thread and hands us CDP messages through a
//! `std::sync::mpsc` channel. We dispatch them to the session at event-loop
//! checkpoints (`poll`) and, while V8 is paused at a breakpoint, in a
//! blocking drain loop (`run_pause_loop`) that V8 enters via
//! `run_message_loop_on_pause`.
//!
//! Reentrancy: `run_message_loop_on_pause` only receives `&self` (the
//! client), but it must dispatch to the session (created later, by
//! `connect`). The shared state below threads a raw pointer to the boxed
//! session so the pause loop can reach it. Everything is single-threaded, so
//! `Cell` suffices and the pointer stays valid for the session's life.

use std::cell::Cell;
use std::net::SocketAddr;
use std::rc::Rc;

use oam_core::inspector::{FromClient, InspectorHandle, OutboundSender};

/// One context group; oam runs a single context per isolate.
const CONTEXT_GROUP_ID: i32 = 1;

/// State shared by the client callback, the channel callback, and the
/// runtime. Single-threaded (isolate thread), so `Cell` is enough.
pub(crate) struct Shared {
    /// The transport; kept alive here so the server thread lives as long as
    /// the inspector. `from_client` (a public field) is the inbound channel.
    handle: InspectorHandle,
    /// Raw pointer to the boxed `V8InspectorSession`, set once after
    /// `connect`. Null until then. Used to dispatch from `&self`-only
    /// callbacks.
    session: Cell<*const v8::inspector::V8InspectorSession>,
    /// Nesting depth of active V8 pauses. A counter, not a bool, because V8
    /// re-enters `run_message_loop_on_pause` when a command dispatched WHILE
    /// paused runs JS that hits another breakpoint (e.g. console
    /// `Runtime.evaluate` lands on a `debugger;`). Each level loops until the
    /// depth drops back below its entry level, so an inner resume never
    /// collapses the outer pause.
    pause_depth: Cell<u32>,
    /// Set when the client sends `Runtime.runIfWaitingForDebugger` — the
    /// signal `--inspect-brk` waits for before starting the program.
    run_requested: Cell<bool>,
}

impl Shared {
    /// Dispatch one CDP message to the V8 session. No-op before `connect`.
    fn dispatch(&self, message: &str) {
        let session = self.session.get();
        if session.is_null() {
            return;
        }
        // UTF-16 is lossless for any input; CDP from DevTools is ASCII-safe
        // but a program name or eval string can carry non-ASCII.
        let utf16: Vec<u16> = message.encode_utf16().collect();
        let view = v8::inspector::StringView::from(&utf16[..]);
        // SAFETY: single-threaded; the session outlives every dispatch (it's
        // boxed and owned by the runtime, dropped only on isolate teardown).
        unsafe { (*session).dispatch_protocol_message(view) };
    }

    /// Forward one outbound CDP message (from the V8 channel) to the client.
    fn send_to_client(&self, message: v8::UniquePtr<v8::inspector::StringBuffer>) {
        if message.is_null() {
            return;
        }
        let buffer = message.unwrap();
        self.handle.send_to_client(format!("{}", buffer.string()));
    }

    /// Drain any pending inbound CDP messages without blocking. Called at
    /// event-loop checkpoints while the program runs.
    pub(crate) fn poll(&self) {
        while let Ok(message) = self.handle.from_client.try_recv() {
            match message {
                FromClient::Message(text) => self.dispatch(&text),
                FromClient::Reconnected(new_tx) => self.apply_reconnect(new_tx),
                FromClient::Connected | FromClient::Disconnected => {}
            }
        }
    }

    /// A new debugger client connected after the previous one left.  Swap the
    /// outbound sender and clear per-session state so `wait_for_attach` and
    /// the pause loop behave correctly for the fresh session.
    fn apply_reconnect(&self, new_tx: OutboundSender) {
        self.handle.swap_sender(new_tx);
        // `run_requested` is set by `Runtime.runIfWaitingForDebugger` which
        // the new client must re-send.  Clear it so --inspect-brk wait loops
        // don't immediately pass on the second attach.
        self.run_requested.set(false);
        // If a prior session left a stale pause (e.g. client vanished mid-
        // break), clear the depth so the engine can make progress while
        // waiting for the new client to re-issue Debugger commands.
        self.pause_depth.set(0);
    }

    /// V8 paused execution: block, dispatching debugger commands, until this
    /// pause level is resumed. Captures its entry level so a NESTED pause's
    /// resume (which decrements once) exits only the inner loop, leaving the
    /// outer pause intact.
    fn run_pause_loop(&self) {
        let level = self.pause_depth.get() + 1;
        self.pause_depth.set(level);
        while self.pause_depth.get() >= level {
            match self.handle.from_client.recv() {
                Ok(FromClient::Message(text)) => self.dispatch(&text),
                Ok(FromClient::Connected) => {}
                // Client vanished while paused — drop all pause levels so the
                // process can make progress / exit rather than hang.
                Ok(FromClient::Disconnected) | Err(_) => self.pause_depth.set(0),
                // New client arrived after disconnect: swap sender and clear
                // the pause so the engine is not stuck waiting for a resume
                // that can never come from the now-disconnected prior client.
                Ok(FromClient::Reconnected(new_tx)) => {
                    self.apply_reconnect(new_tx);
                    // pause_depth already zeroed by apply_reconnect.
                }
            }
        }
    }

    /// End the innermost pause level (V8 calls this on `Debugger.resume`/step).
    fn quit_pause(&self) {
        self.pause_depth
            .set(self.pause_depth.get().saturating_sub(1));
    }

    /// `--inspect-brk`, part 1: block until a debugger attaches and sends
    /// `Runtime.runIfWaitingForDebugger`. Returns false if the client gave up
    /// (run uninspected). Call BEFORE loading the module graph so breakpoints
    /// can be set before any code — including dependencies — runs.
    pub(crate) fn wait_for_attach(&self) -> bool {
        while !self.run_requested.get() {
            match self.handle.from_client.recv() {
                Ok(FromClient::Message(text)) => self.dispatch(&text),
                Ok(FromClient::Connected) => {}
                Ok(FromClient::Disconnected) | Err(_) => return false,
                // A reconnect while waiting: swap sender and keep waiting for
                // the new client to send runIfWaitingForDebugger.
                Ok(FromClient::Reconnected(new_tx)) => self.apply_reconnect(new_tx),
            }
        }
        true
    }

    /// `--inspect-brk`, part 2: schedule a pause on the next statement V8
    /// executes. Call AFTER the module graph is loaded (graph load eagerly
    /// runs CJS dependency bodies) and just before the entry evaluates, so
    /// break-on-start lands on the entry's first line, not inside node_modules.
    pub(crate) fn arm_break(&self) {
        let session = self.session.get();
        if !session.is_null() {
            let reason = v8::inspector::StringView::from(&b"Break on start"[..]);
            let detail = v8::inspector::StringView::empty();
            // SAFETY: as in `dispatch`.
            unsafe { (*session).schedule_pause_on_next_statement(reason, detail) };
        }
    }
}

/// The `V8InspectorClientImpl`: routes V8's pause/run callbacks into shared
/// state.
struct Client {
    shared: Rc<Shared>,
}

impl v8::inspector::V8InspectorClientImpl for Client {
    fn run_message_loop_on_pause(&self, _context_group_id: i32) {
        self.shared.run_pause_loop();
    }
    fn quit_message_loop_on_pause(&self) {
        self.shared.quit_pause();
    }
    fn run_if_waiting_for_debugger(&self, _context_group_id: i32) {
        self.shared.run_requested.set(true);
    }
}

/// The `ChannelImpl`: forwards V8's outbound protocol messages to the client.
struct Channel {
    shared: Rc<Shared>,
}

impl v8::inspector::ChannelImpl for Channel {
    fn send_response(&self, _call_id: i32, message: v8::UniquePtr<v8::inspector::StringBuffer>) {
        self.shared.send_to_client(message);
    }
    fn send_notification(&self, message: v8::UniquePtr<v8::inspector::StringBuffer>) {
        self.shared.send_to_client(message);
    }
    fn flush_protocol_notifications(&self) {}
}

/// Lives in an isolate slot so `pump_event_loop` (which only holds a scope)
/// can poll the transport without threading the runtime through.
pub(crate) struct InspectorSlot(pub Rc<Shared>);

/// The owned inspector state. The runtime drops it (via `teardown`) with the
/// isolate still entered, before the `OwnedIsolate` is disposed —
/// `V8Inspector`/`V8InspectorSession` Drop call back into V8.
///
/// Field order is load-bearing: `session` is declared BEFORE `inspector` so
/// the session's C++ destructor (which deregisters from the inspector) runs
/// while the inspector is still alive. The reverse order is a use-after-free.
pub(crate) struct InspectorState {
    /// Boxed so its address is stable — `Shared::session` points at it. Held
    /// only to own the session (RAII); read solely through that raw pointer,
    /// and dropped (session-before-inspector) on teardown.
    #[allow(dead_code)]
    session: Box<v8::inspector::V8InspectorSession>,
    /// Keeps the V8 inspector alive (owns the client callback).
    inspector: v8::inspector::V8Inspector,
    shared: Rc<Shared>,
    /// `--inspect-brk`: wait for a debugger and break on the first statement.
    break_on_start: bool,
}

impl InspectorState {
    pub(crate) fn shared(&self) -> Rc<Shared> {
        self.shared.clone()
    }
    pub(crate) fn break_on_start(&self) -> bool {
        self.break_on_start
    }

    /// Tear down with the isolate entered (the caller holds a live scope):
    /// unregister the context, then drop — session deletes first, then the
    /// inspector, both with the isolate current.
    pub(crate) fn teardown(self, context: v8::Local<'_, v8::Context>) {
        self.inspector.context_destroyed(context);
        // `self` drops here: session DELETE, then inspector DELETE.
    }
}

/// Build the inspector: start the transport, create the V8 inspector +
/// session, and register `context`. Returns the state (for the runtime to
/// own) and the `ws://` URL for the CLI to announce.
pub(crate) fn attach(
    isolate: &mut v8::Isolate,
    context: &v8::Global<v8::Context>,
    addr: SocketAddr,
    break_on_start: bool,
) -> anyhow::Result<(InspectorState, String)> {
    let uuid = random_uuid();
    let handle = oam_core::inspector::start(addr, uuid)?;
    let ws_url = handle.ws_url();

    let shared = Rc::new(Shared {
        handle,
        session: Cell::new(std::ptr::null()),
        pause_depth: Cell::new(0),
        run_requested: Cell::new(false),
    });

    let client = v8::inspector::V8InspectorClient::new(Box::new(Client {
        shared: shared.clone(),
    }));
    let inspector = v8::inspector::V8Inspector::create(isolate, client);

    let channel = v8::inspector::Channel::new(Box::new(Channel {
        shared: shared.clone(),
    }));
    let session = inspector.connect(
        CONTEXT_GROUP_ID,
        channel,
        v8::inspector::StringView::empty(),
        v8::inspector::V8InspectorClientTrustLevel::FullyTrusted,
    );
    let session = Box::new(session);
    shared.session.set(&*session as *const _);

    // Register the context so the Debugger/Runtime domains can see it.
    {
        v8::scope_with_context!(let scope, isolate, context);
        let context_local = v8::Local::new(scope, context);
        let name = v8::inspector::StringView::from(&b"oam"[..]);
        let aux = v8::inspector::StringView::from(&b"{\"isDefault\":true}"[..]);
        inspector.context_created(context_local, CONTEXT_GROUP_ID, name, aux);
    }

    Ok((
        InspectorState {
            session,
            inspector,
            shared,
            break_on_start,
        },
        ws_url,
    ))
}

/// A v4-shaped random id for the debugger URL. DevTools only needs
/// uniqueness, not RFC-4122 conformance.
fn random_uuid() -> String {
    let mut bytes = [0u8; 16];
    // Best-effort OS randomness; a failure here only weakens uniqueness of a
    // localhost debug URL, so fall back to a fixed pattern rather than error.
    let _ = getrandom::fill(&mut bytes);
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}
