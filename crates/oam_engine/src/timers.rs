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
use std::collections::{BinaryHeap, HashMap};
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

pub(crate) struct TimerQueue {
    next_id: u32,
    seq: u64,
    /// Min-heap of (deadline, insertion seq, id). seq keeps same-deadline
    /// timers FIFO. Cancelled ids are skipped lazily (absent from `active`).
    heap: BinaryHeap<Reverse<(Instant, u64, u32)>>,
    active: HashMap<u32, TimerEntry>,
    /// Count of live `active` timers that are ref'd. O(1) "does any ref'd
    /// timer keep the loop alive?" check for the event loop's exit decision,
    /// kept in sync by schedule / cancel / pop_due / set_ref.
    ref_count: usize,
}

impl Default for TimerQueue {
    fn default() -> Self {
        Self {
            next_id: 1, // ids start at 1, like Node — 0 stays falsy-safe
            seq: 0,
            heap: BinaryHeap::new(),
            active: HashMap::new(),
            ref_count: 0,
        }
    }
}

impl TimerQueue {
    fn schedule(&mut self, entry: TimerEntry, delay: Duration) -> u32 {
        let mut id = self.next_id;
        while self.active.contains_key(&id) {
            id = id.wrapping_add(1).max(1);
        }
        self.next_id = id.wrapping_add(1).max(1);
        self.seq += 1;
        self.heap
            .push(Reverse((Instant::now() + delay, self.seq, id)));
        if entry.is_ref {
            self.ref_count += 1;
        }
        self.active.insert(id, entry);
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
        if self.heap.len() > 64 && self.heap.len() > live * 2 {
            self.heap
                .retain(|Reverse((_, _, id))| self.active.contains_key(id));
        }
    }

    /// Next deadline among live timers, lazily discarding cancelled entries.
    pub(crate) fn next_deadline(&mut self) -> Option<Instant> {
        while let Some(&Reverse((deadline, _, id))) = self.heap.peek() {
            if self.active.contains_key(&id) {
                return Some(deadline);
            }
            self.heap.pop();
        }
        None
    }

    /// Pop ONE due timer. Intervals reschedule themselves; one-shots are
    /// removed from `active` before their callback runs (Node behavior).
    pub(crate) fn pop_due(
        &mut self,
        now: Instant,
    ) -> Option<(v8::Global<v8::Function>, Vec<v8::Global<v8::Value>>)> {
        loop {
            let &Reverse((deadline, _, id)) = self.heap.peek()?;
            if deadline > now {
                return None;
            }
            self.heap.pop();
            let Some(entry) = self.active.get(&id) else {
                continue; // cancelled
            };
            let callback = entry.callback.clone();
            let args = entry.args.clone();
            let interval = entry.interval;
            let is_ref = entry.is_ref;
            if let Some(period) = interval {
                self.seq += 1;
                self.heap.push(Reverse((now + period, self.seq, id)));
            } else {
                self.active.remove(&id);
                if is_ref {
                    self.ref_count -= 1;
                }
            }
            return Some((callback, args));
        }
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

    /// Whether any live timer is ref'd. The event loop stays alive only for
    /// ref'd timers (and inflight ops); when this is false and no ops remain,
    /// the loop exits WITHOUT firing the remaining unref'd timers.
    pub(crate) fn has_ref_timers(&self) -> bool {
        self.ref_count > 0
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
