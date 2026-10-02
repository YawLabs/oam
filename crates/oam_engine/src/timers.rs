//! Timers and the M1 blocking event loop.
//!
//! setTimeout / setInterval / clearTimeout / clearInterval / queueMicrotask,
//! serviced by a min-heap timer queue in an isolate slot (the JS bindings
//! must be zero-capture functions). `execute_module` drives the loop:
//! pop one due timer, call it under the TryCatch, drain microtasks, repeat;
//! sleep until the next deadline when idle; exit when no timers remain.
//!
//! One-timer-at-a-time (no batching) so a clearTimeout() issued by one
//! callback reliably cancels a not-yet-fired timer due in the same instant.
//! The tokio-backed loop with IO lands next in oam_core; this slice makes
//! TLA-on-timers and the standard timing globals real.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::time::{Duration, Instant};

pub(crate) struct TimerEntry {
    callback: v8::Global<v8::Function>,
    args: Vec<v8::Global<v8::Value>>,
    /// Some(period) for setInterval, None for setTimeout.
    interval: Option<Duration>,
    /// Node ref/unref: a ref'd timer keeps the event loop alive; an unref'd
    /// one does not (it still FIRES while other work keeps the loop open, but
    /// when it is the sole remaining work the loop exits without running it).
    /// New timers start ref'd, matching Node.
    is_ref: bool,
}

/// A callback queued by [`timer_pending`]: libuv's pending phase.
struct PendingEntry {
    callback: v8::Global<v8::Function>,
    args: Vec<v8::Global<v8::Value>>,
    gate: PendingGate,
}

/// What a pending callback still waits for, from the phase that queued it.
/// Node's loop runs timers, then pending callbacks, then (after the poll)
/// immediates, and each phase runs only the entries due when it started.
/// oam's loop pops one due entry per turn, so a pending callback queued from
/// inside a phase records what is left of that phase and waits it out.
#[derive(Clone, Copy)]
enum PendingGate {
    /// Runs at the next turn, before any timer or immediate: queued from the
    /// main script, an I/O callback, or the ticks and microtasks after one.
    Open,
    /// Queued in the timers phase: waits for the timers due at or before
    /// this instant that are due by now (node's phase runs every timer
    /// expired at the loop time it started with), and no immediate.
    Timers(Instant),
    /// Queued in the check phase: waits for the immediates queued at or
    /// before this instant (node's check phase runs the immediates queued
    /// when it started; one queued during it runs on the next loop
    /// iteration), then for the timers due when they are done -- node runs
    /// its timers phase before the next iteration's pending phase.
    Immediates(Instant),
}

/// The timer or immediate whose callback is running, with the ticks and
/// microtasks after it: the phase a pending callback queued now belongs to.
#[derive(Clone, Copy)]
enum Running {
    Timer { now: Instant, deadline: Instant },
    Immediate { now: Instant },
}

pub(crate) struct TimerQueue {
    next_id: u32,
    seq: u64,
    /// When the queue was made: the deadline [`Self::next_deadline`] reports
    /// while a pending callback waits, earlier than any timer's, so the loop
    /// does not sleep. (A pending callback is never waiting on anything that
    /// is not due: see [`PendingGate`].)
    epoch: Instant,
    /// Min-heap of (deadline, insertion seq, id) for setTimeout and
    /// setInterval. seq keeps same-deadline timers FIFO. Cancelled ids are
    /// skipped lazily (absent from `active`).
    heap: BinaryHeap<Reverse<(Instant, u64, u32)>>,
    /// setImmediate entries as (queued at, seq, id), in the order queued --
    /// which is also deadline order, so a FIFO is the min-heap. Kept apart
    /// from `heap` so a pending callback can wait for one kind and not the
    /// other; together they still pop in (deadline, seq) order.
    immediates: VecDeque<(Instant, u64, u32)>,
    active: HashMap<u32, TimerEntry>,
    /// Count of live `active` timers that are ref'd. O(1) "does any ref'd
    /// timer keep the loop alive?" check for the event loop's exit decision,
    /// kept in sync by schedule / cancel / pop_due / set_ref.
    ref_count: usize,
    /// [`timer_pending`] callbacks, FIFO; the front's gate holds the rest.
    pending: VecDeque<PendingEntry>,
    /// Set by `pop_due` for a timer or immediate, cleared by
    /// [`Self::callback_done`] once its callback and drains are over.
    running: Option<Running>,
}

impl Default for TimerQueue {
    fn default() -> Self {
        Self {
            next_id: 1, // ids start at 1, like Node — 0 stays falsy-safe
            seq: 0,
            epoch: Instant::now(),
            heap: BinaryHeap::new(),
            immediates: VecDeque::new(),
            active: HashMap::new(),
            ref_count: 0,
            pending: VecDeque::new(),
            running: None,
        }
    }
}

impl TimerQueue {
    fn insert(&mut self, entry: TimerEntry) -> (u32, u64) {
        let mut id = self.next_id;
        while self.active.contains_key(&id) {
            id = id.wrapping_add(1).max(1);
        }
        self.next_id = id.wrapping_add(1).max(1);
        self.seq += 1;
        if entry.is_ref {
            self.ref_count += 1;
        }
        self.active.insert(id, entry);
        (id, self.seq)
    }

    fn schedule(&mut self, entry: TimerEntry, delay: Duration) -> u32 {
        let deadline = Instant::now() + delay;
        let (id, seq) = self.insert(entry);
        self.heap.push(Reverse((deadline, seq, id)));
        id
    }

    fn schedule_immediate(&mut self, entry: TimerEntry) -> u32 {
        let queued = Instant::now();
        let (id, seq) = self.insert(entry);
        self.immediates.push_back((queued, seq, id));
        id
    }

    fn cancel(&mut self, id: u32) {
        let Some(entry) = self.active.remove(&id) else {
            return;
        };
        if entry.is_ref {
            self.ref_count -= 1;
        }
        // The heap entry stays until its deadline reaches the front, so a
        // cancelled far-future timer (the ubiquitous set-then-clear-on-
        // success pattern) would sit in the heap for the whole timeout
        // window — unbounded growth on a long-running server. When dead
        // entries dominate, rebuild the heap from the live set. Amortized
        // O(1): the rebuild cost is paid against the dead entries removed.
        let live = self.active.len();
        let queued = self.heap.len() + self.immediates.len();
        if queued > 64 && queued > live * 2 {
            self.heap
                .retain(|Reverse((_, _, id))| self.active.contains_key(id));
            self.immediates
                .retain(|(_, _, id)| self.active.contains_key(id));
        }
    }

    /// The front live timer, lazily discarding cancelled entries.
    fn front_timer(&mut self) -> Option<(Instant, u64)> {
        while let Some(&Reverse((deadline, seq, id))) = self.heap.peek() {
            if self.active.contains_key(&id) {
                return Some((deadline, seq));
            }
            self.heap.pop();
        }
        None
    }

    /// The front live immediate, lazily discarding cleared entries.
    fn front_immediate(&mut self) -> Option<(Instant, u64)> {
        while let Some(&(queued, seq, id)) = self.immediates.front() {
            if self.active.contains_key(&id) {
                return Some((queued, seq));
            }
            self.immediates.pop_front();
        }
        None
    }

    /// Next deadline among live timers and immediates, or a past instant
    /// while a pending callback waits (something is runnable at once).
    pub(crate) fn next_deadline(&mut self) -> Option<Instant> {
        if !self.pending.is_empty() {
            return Some(self.epoch);
        }
        match (self.front_timer(), self.front_immediate()) {
            (Some(timer), Some(immediate)) => Some(timer.min(immediate).0),
            (Some((deadline, _)), None) | (None, Some((deadline, _))) => Some(deadline),
            (None, None) => None,
        }
    }

    /// Pop ONE due callback: a pending one whose phase is over, else the due
    /// timer or immediate with the earliest (deadline, seq). Intervals
    /// reschedule themselves; one-shots are removed from `active` before
    /// their callback runs (Node behavior).
    pub(crate) fn pop_due(
        &mut self,
        now: Instant,
    ) -> Option<(v8::Global<v8::Function>, Vec<v8::Global<v8::Value>>)> {
        self.running = None;
        while let Some(gate) = self.pending.front().map(|pending| pending.gate) {
            let next = match gate {
                PendingGate::Open => {
                    let pending = self.pending.pop_front()?;
                    return Some((pending.callback, pending.args));
                }
                PendingGate::Immediates(until) => {
                    if self
                        .front_immediate()
                        .is_some_and(|(queued, _)| queued <= until)
                    {
                        return self.pop_immediate(now);
                    }
                    PendingGate::Timers(now)
                }
                PendingGate::Timers(until) => {
                    if self
                        .front_timer()
                        .is_some_and(|(deadline, _)| deadline <= until.min(now))
                    {
                        return self.pop_timer(now);
                    }
                    PendingGate::Open
                }
            };
            if let Some(pending) = self.pending.front_mut() {
                pending.gate = next;
            }
        }
        let timer = self.front_timer().filter(|&(deadline, _)| deadline <= now);
        let immediate = self.front_immediate().filter(|&(queued, _)| queued <= now);
        match (timer, immediate) {
            (Some(timer), Some(immediate)) if immediate < timer => self.pop_immediate(now),
            (Some(_), _) => self.pop_timer(now),
            (None, Some(_)) => self.pop_immediate(now),
            (None, None) => None,
        }
    }

    /// Pop the front timer (live: the caller just peeked it).
    fn pop_timer(
        &mut self,
        now: Instant,
    ) -> Option<(v8::Global<v8::Function>, Vec<v8::Global<v8::Value>>)> {
        let Reverse((deadline, _, id)) = self.heap.pop()?;
        let entry = self.active.get(&id)?;
        let callback = entry.callback.clone();
        let args = entry.args.clone();
        if let Some(period) = entry.interval {
            self.seq += 1;
            self.heap.push(Reverse((now + period, self.seq, id)));
        } else if self.active.remove(&id).is_some_and(|entry| entry.is_ref) {
            self.ref_count -= 1;
        }
        self.running = Some(Running::Timer { now, deadline });
        Some((callback, args))
    }

    /// Pop the front immediate (live: the caller just peeked it).
    fn pop_immediate(
        &mut self,
        now: Instant,
    ) -> Option<(v8::Global<v8::Function>, Vec<v8::Global<v8::Value>>)> {
        let (_, _, id) = self.immediates.pop_front()?;
        let entry = self.active.remove(&id)?;
        if entry.is_ref {
            self.ref_count -= 1;
        }
        self.running = Some(Running::Immediate { now });
        Some((entry.callback, entry.args))
    }

    /// The callback `pop_due` returned has run, with the ticks and
    /// microtasks after it: a pending callback queued from here on belongs
    /// to no timer's or immediate's phase.
    pub(crate) fn callback_done(&mut self) {
        self.running = None;
    }

    /// Flip a live timer's ref flag (Node's Timeout#ref / #unref). Unknown or
    /// already-fired/cancelled ids are a no-op. Keeps `ref_count` in sync.
    pub(crate) fn set_ref(&mut self, id: u32, value: bool) {
        if let Some(entry) = self.active.get_mut(&id)
            && entry.is_ref != value
        {
            entry.is_ref = value;
            if value {
                self.ref_count += 1;
            } else {
                self.ref_count -= 1;
            }
        }
    }

    /// Whether any live timer is ref'd, or a pending callback waits. The
    /// event loop stays alive only for these (and inflight ops); when this
    /// is false and no ops remain, the loop exits WITHOUT firing the
    /// remaining unref'd timers.
    pub(crate) fn has_ref_timers(&self) -> bool {
        self.ref_count > 0 || !self.pending.is_empty()
    }
}

/// Install the timing globals onto `global`. Called once per context.
pub(crate) fn install(scope: &mut v8::PinScope<'_, '_>, context: v8::Local<v8::Context>) {
    let global = context.global(scope);
    let bindings: [(&str, v8::Local<v8::Function>); 5] = [
        ("setTimeout", v8::Function::new(scope, set_timeout).unwrap()),
        (
            "setInterval",
            v8::Function::new(scope, set_interval).unwrap(),
        ),
        (
            "clearTimeout",
            v8::Function::new(scope, clear_timer).unwrap(),
        ),
        (
            "clearInterval",
            v8::Function::new(scope, clear_timer).unwrap(),
        ),
        (
            "queueMicrotask",
            v8::Function::new(scope, queue_microtask).unwrap(),
        ),
    ];
    for (name, function) in bindings {
        let key = v8::String::new(scope, name).unwrap();
        global.set(scope, key.into(), function.into());
    }
}

fn throw_type_error(scope: &mut v8::PinScope<'_, '_>, message: &str) {
    let message = v8::String::new(scope, message).unwrap();
    let exception = v8::Exception::type_error(scope, message);
    scope.throw_exception(exception);
}

fn schedule_from_args(
    scope: &mut v8::PinScope<'_, '_>,
    args: &v8::FunctionCallbackArguments<'_>,
    rv: &mut v8::ReturnValue<'_, v8::Value>,
    repeating: bool,
) {
    let Ok(callback) = v8::Local::<v8::Function>::try_from(args.get(0)) else {
        throw_type_error(scope, "setTimeout/setInterval callback must be a function");
        return;
    };
    let ms = args.get(1).number_value(scope).unwrap_or(0.0);
    // Node parity: delays clamp to a 1ms minimum. Also kills the 0ms-interval
    // busy-spin (review finding: continuously-due timers starved op completions).
    let ms = if ms.is_finite() && ms > 1.0 { ms } else { 1.0 };
    let delay = Duration::from_millis(ms as u64);

    let callback = v8::Global::new(scope, callback);
    let mut extra = Vec::new();
    for i in 2..args.length() {
        extra.push(v8::Global::new(scope, args.get(i)));
    }

    let entry = TimerEntry {
        callback,
        args: extra,
        interval: repeating.then_some(delay),
        is_ref: true, // Node: timers start ref'd; JS calls timerUnref to clear it
    };
    let id = scope
        .get_slot_mut::<TimerQueue>()
        .expect("timer queue installed")
        .schedule(entry, delay);
    rv.set_uint32(id);
}

fn set_timeout(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    schedule_from_args(scope, &args, &mut rv, false);
}

fn set_interval(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    schedule_from_args(scope, &args, &mut rv, true);
}

fn clear_timer(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    if let Some(id) = args.get(0).uint32_value(scope)
        && let Some(queue) = scope.get_slot_mut::<TimerQueue>()
    {
        queue.cancel(id);
    }
}

/// `__oam.node.timerRef(id)` — Node's Timeout#ref. Marks a live timer as ref'd
/// so it keeps the event loop alive. Backs the JS Timeout wrapper's .ref().
pub(crate) fn timer_ref(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    if let Some(id) = args.get(0).uint32_value(scope)
        && let Some(queue) = scope.get_slot_mut::<TimerQueue>()
    {
        queue.set_ref(id, true);
    }
}

/// `__oam.node.timerUnref(id)` — Node's Timeout#unref. Marks a live timer as
/// unref'd so it no longer keeps the event loop alive (it still fires while
/// other work keeps the loop open). Backs the JS Timeout wrapper's .unref().
pub(crate) fn timer_unref(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    if let Some(id) = args.get(0).uint32_value(scope)
        && let Some(queue) = scope.get_slot_mut::<TimerQueue>()
    {
        queue.set_ref(id, false);
    }
}

/// `__oam.node.timerImmediate(callback, ...args)`: node's setImmediate, a
/// callback due at once -- it runs on the loop's next turn, after the op
/// completions already waiting, never after a wait on the OS timer. (A
/// setTimeout clamps to node's 1 ms, and on Windows an idle loop then
/// sleeps a whole timer tick, about 15 ms, for every setImmediate.) Shares
/// the timer queue, so clearImmediate / ref / unref work by id.
pub(crate) fn timer_immediate(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Ok(callback) = v8::Local::<v8::Function>::try_from(args.get(0)) else {
        throw_type_error(scope, "setImmediate callback must be a function");
        return;
    };
    let callback = v8::Global::new(scope, callback);
    let mut extra = Vec::new();
    for i in 1..args.length() {
        extra.push(v8::Global::new(scope, args.get(i)));
    }
    let entry = TimerEntry {
        callback,
        args: extra,
        interval: None,
        is_ref: true,
    };
    let id = scope
        .get_slot_mut::<TimerQueue>()
        .expect("timer queue installed")
        .schedule_immediate(entry);
    rv.set_uint32(id);
}

/// `__oam.node.timerPending(callback, ...args)`: libuv's pending phase, a
/// callback node's loop runs after every tick and microtask the current
/// callback queued, and after what is left of the phase it was queued in:
///
/// * from the main script, an I/O callback, or a pending callback: at the
///   next turn, before any timer or immediate (node's pending phase follows
///   the poll and comes before the check phase);
/// * from a timer: after the other timers due in that timers phase, and
///   before any immediate;
/// * from an immediate: after the other immediates queued before that check
///   phase began, and the timers due once they are done (node's next
///   iteration runs its timers phase first), and before the immediates
///   queued during it.
///
/// Measured against node v22.22.2 (conformance case 264). Pending callbacks
/// run in the order they were queued, one per turn as timers do, and keep
/// the loop alive until they have run.
///
/// For a request oam's natives finished inside the call that made it -- a
/// socket's shutdown with no write queued -- whose completion node still
/// reports from the loop: net.Socket's 'finish'.
pub(crate) fn timer_pending(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Ok(callback) = v8::Local::<v8::Function>::try_from(args.get(0)) else {
        throw_type_error(scope, "timerPending callback must be a function");
        return;
    };
    let callback = v8::Global::new(scope, callback);
    let mut extra = Vec::new();
    for i in 1..args.length() {
        extra.push(v8::Global::new(scope, args.get(i)));
    }
    let queue = scope
        .get_slot_mut::<TimerQueue>()
        .expect("timer queue installed");
    let gate = match queue.running {
        None => PendingGate::Open,
        // Node's timers phase runs every timer expired at the loop time it
        // started with, in whole milliseconds, and node starts the timers
        // one piece of code sets from the same cached loop time, so two
        // setTimeout(fn, 5) in a row share an expiry. oam's deadlines come
        // from the clock at each call, microseconds apart: a timer within
        // the millisecond of the running one's deadline is due with it.
        Some(Running::Timer { now, deadline }) => {
            PendingGate::Timers(now.max(deadline + Duration::from_nanos(999_999)))
        }
        Some(Running::Immediate { now }) => PendingGate::Immediates(now),
    };
    queue.pending.push_back(PendingEntry {
        callback,
        args: extra,
        gate,
    });
}

fn queue_microtask(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Ok(callback) = v8::Local::<v8::Function>::try_from(args.get(0)) else {
        throw_type_error(scope, "queueMicrotask requires a function");
        return;
    };
    scope.enqueue_microtask(callback);
}
