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
}

pub(crate) struct TimerQueue {
    next_id: u32,
    seq: u64,
    /// Min-heap of (deadline, insertion seq, id). seq keeps same-deadline
    /// timers FIFO. Cancelled ids are skipped lazily (absent from `active`).
    heap: BinaryHeap<Reverse<(Instant, u64, u32)>>,
    active: HashMap<u32, TimerEntry>,
}

impl Default for TimerQueue {
    fn default() -> Self {
        Self {
            next_id: 1, // ids start at 1, like Node — 0 stays falsy-safe
            seq: 0,
            heap: BinaryHeap::new(),
            active: HashMap::new(),
        }
    }
}

impl TimerQueue {
    fn schedule(&mut self, entry: TimerEntry, delay: Duration) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.seq += 1;
        self.heap
            .push(Reverse((Instant::now() + delay, self.seq, id)));
        self.active.insert(id, entry);
        id
    }

    fn cancel(&mut self, id: u32) {
        if self.active.remove(&id).is_none() {
            return;
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
            if let Some(period) = entry.interval {
                self.seq += 1;
                self.heap.push(Reverse((now + period, self.seq, id)));
            } else {
                self.active.remove(&id);
            }
            return Some((callback, args));
        }
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
