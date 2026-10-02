//! V8's foreground tasks, run on the event loop that owns the isolate.
//!
//! V8 hands the embedder work that must run on an isolate's own thread: the
//! task that settles an async `WebAssembly.compile` / `instantiate` once the
//! background compile finishes, the FinalizationRegistry cleanup task, an
//! `Atomics.waitAsync` notify or timeout, and the GC's idle-time and
//! memory-reducer tasks. With V8's default platform those tasks sit in a queue
//! only `v8::platform::PumpMessageLoop` drains, and nothing in oam ever pumped
//! it -- so `await WebAssembly.compile(bytes)` never settled. That is what hung
//! undici 6 from npm: its HTTP/1 client awaits the async compile of its llhttp
//! parser before the first request goes out.
//!
//! Node's platform (src/node_platform.cc) runs these tasks from libuv: a
//! posted task signals an unref'd uv_async on the isolate's loop, a delayed
//! task arms an unref'd uv_timer, and each task runs inside an
//! InternalCallbackScope, so ticks and microtasks drain after it. Neither
//! keeps the process alive; only `DrainTasks` at the end of every loop
//! iteration does, by blocking until V8's in-flight background work is done.
//!
//! oam does the same over its op channel. [`OamPlatform`] is a
//! `v8::PlatformImpl`: each posted task is queued on its isolate's
//! [`IsolateTasks`] and the isolate's loop is woken with a
//! [`oam_core::LoopWaker`] (an uncounted op-channel message). Delayed tasks
//! wait on their isolate's own heap; one process-wide timer thread only ever
//! decides WHEN to promote them, so a task never leaves its isolate's lock and
//! is never destroyed on a foreign thread after the isolate is gone. The loop
//! (`modules::run_platform_tasks`) runs ready tasks at the top of each turn,
//! and at the would-exit point waits while `Isolate::has_pending_background_
//! tasks` says V8 still owes it one -- node's DrainTasks.

use std::cmp::Ordering as CmpOrdering;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{Duration, Instant};

/// An isolate's address: the key V8 posts tasks under. Used only as an
/// opaque map key, never dereferenced.
type IsolateKey = usize;

/// Every live isolate's task queues, by address.
fn registry() -> &'static Mutex<HashMap<IsolateKey, Arc<IsolateTasks>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<IsolateKey, Arc<IsolateTasks>>>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

/// A poisoned lock only means another thread panicked mid-push; the queue
/// itself is still a valid VecDeque/HashMap, and V8 tasks must still run.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

thread_local! {
    /// Set on a thread while it runs `Isolate::new` (see [`new_isolate`]).
    /// Every post that thread makes in that span is for the isolate being
    /// built, never for an older one.
    static CREATING_ISOLATE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The isolate's queues for `key`, created on first use: V8 can post while
/// `Isolate::new` is still running, before [`register`] has the address.
///
/// A closed entry is a dead isolate's tombstone. It stays mapped from
/// [`Registration::close`] until [`Registration::drop`], which runs only
/// after the isolate is disposed and freed, and it swallows any task posted
/// for that isolate during its disposal. If the allocator hands the freed
/// address to a new isolate inside that window, the new isolate's posts
/// would land in the tombstone and be destroyed, and its loop would read a
/// queue nothing runs. Only the thread building the new isolate can tell the
/// two apart, because only that thread posts for an address the allocator
/// has just handed out. So a post from that thread, and [`register`], swap a
/// tombstone for a fresh queue. Any other thread's post to a tombstone is
/// the dead isolate's and is still dropped.
fn tasks_for(key: IsolateKey) -> Arc<IsolateTasks> {
    queue_for(key, CREATING_ISOLATE.with(std::cell::Cell::get))
}

/// [`tasks_for`]'s registry step. `fresh_if_closed` replaces a closed entry
/// with a new queue instead of returning it.
fn queue_for(key: IsolateKey, fresh_if_closed: bool) -> Arc<IsolateTasks> {
    use std::collections::hash_map::Entry;
    match lock(registry()).entry(key) {
        Entry::Occupied(mut slot) => {
            if fresh_if_closed && slot.get().is_closed() {
                slot.insert(Arc::default());
            }
            slot.get().clone()
        }
        Entry::Vacant(slot) => slot.insert(Arc::default()).clone(),
    }
}

/// Build an isolate and register its task queues: the one way oam creates
/// an isolate that runs on [`OamPlatform`].
pub(crate) fn new_isolate(params: v8::CreateParams) -> (v8::OwnedIsolate, Registration) {
    /// Clears the flag even if `Isolate::new` unwinds.
    struct Creating;
    impl Drop for Creating {
        fn drop(&mut self) {
            CREATING_ISOLATE.with(|flag| flag.set(false));
        }
    }
    CREATING_ISOLATE.with(|flag| flag.set(true));
    let creating = Creating;
    let mut isolate = v8::Isolate::new(params);
    drop(creating);
    let registration = register(&mut isolate);
    (isolate, registration)
}

/// The platform V8 is initialized with (see `init_platform_with_flags`).
pub(crate) struct OamPlatform;

impl v8::PlatformImpl for OamPlatform {
    fn post_task(&self, isolate_ptr: *mut c_void, task: v8::Task) {
        tasks_for(isolate_ptr as IsolateKey).post(task);
    }

    // Non-nestable only forbids running inside a NESTED message-loop pump.
    // oam never nests one: the loop that runs these tasks is entered only
    // from the embedder (run / REPL / worker), never from under a JS frame.
    fn post_non_nestable_task(&self, isolate_ptr: *mut c_void, task: v8::Task) {
        tasks_for(isolate_ptr as IsolateKey).post(task);
    }

    fn post_delayed_task(&self, isolate_ptr: *mut c_void, task: v8::Task, delay_in_seconds: f64) {
        IsolateTasks::post_delayed(
            &tasks_for(isolate_ptr as IsolateKey),
            task,
            delay_in_seconds,
        );
    }

    fn post_non_nestable_delayed_task(
        &self,
        isolate_ptr: *mut c_void,
        task: v8::Task,
        delay_in_seconds: f64,
    ) {
        IsolateTasks::post_delayed(
            &tasks_for(isolate_ptr as IsolateKey),
            task,
            delay_in_seconds,
        );
    }

    // The platform is created with idle-task support off, so V8 never posts
    // one; dropping is what the default runner does with idle tasks disabled.
    fn post_idle_task(&self, _isolate_ptr: *mut c_void, _task: v8::IdleTask) {}
}

/// One isolate's foreground tasks.
#[derive(Default)]
pub(crate) struct IsolateTasks {
    state: Mutex<TaskState>,
    /// Set when a task becomes ready, cleared when the loop takes the queue:
    /// the loop's per-turn check is this one load, and a wake is sent only
    /// on its false -> true edge so a burst of posts costs one message.
    ready: AtomicBool,
}

#[derive(Default)]
struct TaskState {
    queue: VecDeque<v8::Task>,
    delayed: BinaryHeap<Delayed>,
    waker: Option<oam_core::LoopWaker>,
    /// The isolate is being torn down: tasks posted from here on are
    /// destroyed on the spot, as V8's own runner does once terminated.
    closed: bool,
}

/// A delayed task, ordered so the BinaryHeap pops the earliest due first
/// (FIFO among equal deadlines).
struct Delayed {
    due: Instant,
    seq: u64,
    task: v8::Task,
}

impl PartialEq for Delayed {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == CmpOrdering::Equal
    }
}
impl Eq for Delayed {}
impl PartialOrd for Delayed {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}
impl Ord for Delayed {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        (other.due, other.seq).cmp(&(self.due, self.seq))
    }
}

static DELAYED_SEQ: AtomicU64 = AtomicU64::new(0);

impl IsolateTasks {
    fn post(&self, task: v8::Task) {
        let mut state = lock(&self.state);
        if state.closed {
            return; // `task` drops here, never run
        }
        state.queue.push_back(task);
        self.signal(&state);
    }

    fn post_delayed(this: &Arc<Self>, task: v8::Task, delay_in_seconds: f64) {
        // V8 clamps to >= 0; NaN or a non-finite delay has no due time to wait for.
        let delay = Duration::try_from_secs_f64(delay_in_seconds).unwrap_or(Duration::ZERO);
        if delay.is_zero() {
            this.post(task);
            return;
        }
        let due = Instant::now() + delay;
        {
            let mut state = lock(&this.state);
            if state.closed {
                return;
            }
            let seq = DELAYED_SEQ.fetch_add(1, Ordering::Relaxed);
            state.delayed.push(Delayed { due, seq, task });
        }
        timer().schedule(due, Arc::downgrade(this));
    }

    /// Move every delayed task due by `now` to the ready queue.
    fn promote_due(&self, now: Instant) {
        let mut state = lock(&self.state);
        let mut moved = false;
        while state.delayed.peek().is_some_and(|next| next.due <= now) {
            if let Some(Delayed { task, .. }) = state.delayed.pop() {
                state.queue.push_back(task);
                moved = true;
            }
        }
        if moved {
            self.signal(&state);
        }
    }

    fn signal(&self, state: &TaskState) {
        if !self.ready.swap(true, Ordering::AcqRel)
            && let Some(waker) = &state.waker
        {
            waker.wake();
        }
    }

    fn is_closed(&self) -> bool {
        lock(&self.state).closed
    }

    /// True when a posted task is waiting to run. One atomic load: this is
    /// the event loop's per-turn check.
    pub(crate) fn has_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    /// Take every ready task, in posting order. A task posted after the take
    /// re-raises `ready` (and wakes the loop) for the next turn.
    pub(crate) fn take_ready(&self) -> VecDeque<v8::Task> {
        self.ready.store(false, Ordering::Release);
        std::mem::take(&mut lock(&self.state).queue)
    }

    /// Route wakes to the op channel the isolate's loop is blocked on now.
    /// A run replaces its CoreRuntime (and so its channel) per execution.
    pub(crate) fn set_waker(&self, waker: oam_core::LoopWaker) {
        let mut state = lock(&self.state);
        state.waker = Some(waker);
        // A task that became ready while no channel was attached still
        // needs its wake.
        if self.ready.load(Ordering::Acquire)
            && let Some(waker) = &state.waker
        {
            waker.wake();
        }
    }

    /// The isolate is about to be disposed: refuse further posts and destroy
    /// what is queued, here on the isolate's thread while it is still alive.
    fn close(&self) {
        let (queue, delayed) = {
            let mut state = lock(&self.state);
            state.closed = true;
            state.waker = None;
            (
                std::mem::take(&mut state.queue),
                std::mem::take(&mut state.delayed),
            )
        };
        self.ready.store(false, Ordering::Release);
        // Dropped outside the lock: a task's destructor is V8 code.
        drop(queue);
        drop(delayed);
    }
}

/// The isolate-slot handle the event loop reads its tasks through.
pub(crate) struct PlatformTasks(pub(crate) Arc<IsolateTasks>);

/// Ties an isolate's [`IsolateTasks`] to its lifetime. Owned by `JsRuntime`
/// and declared AFTER the isolate, so it is dropped after the isolate is
/// disposed; [`Registration::close`] runs before, from `JsRuntime::drop`.
pub(crate) struct Registration {
    key: IsolateKey,
    tasks: Arc<IsolateTasks>,
}

/// Register `isolate` (right after `Isolate::new`, on its thread; see
/// [`new_isolate`]) and give its loop the handle to its tasks. A closed
/// entry under its address is a predecessor's tombstone (see [`tasks_for`])
/// and is replaced, so the new isolate never holds a closed queue.
fn register(isolate: &mut v8::OwnedIsolate) -> Registration {
    // SAFETY: as_raw_isolate_ptr only reads the isolate's address; it is
    // used as an opaque map key, compared against the address V8 passes to
    // the PlatformImpl callbacks, and never dereferenced. UnsafeRawIsolatePtr
    // is repr(transparent) over that pointer, so the transmute is its value.
    let key = unsafe {
        std::mem::transmute::<v8::UnsafeRawIsolatePtr, *mut c_void>(isolate.as_raw_isolate_ptr())
    } as IsolateKey;
    let tasks = queue_for(key, true);
    isolate.set_slot(PlatformTasks(tasks.clone()));
    Registration { key, tasks }
}

impl Registration {
    pub(crate) fn close(&self) {
        self.tasks.close();
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.tasks.close();
        let mut map = lock(registry());
        if map
            .get(&self.key)
            .is_some_and(|tasks| Arc::ptr_eq(tasks, &self.tasks))
        {
            map.remove(&self.key);
        }
    }
}

/// The one thread that promotes delayed tasks when they fall due. It holds
/// only Weak handles and deadlines -- never a task.
struct Timer {
    wakes: Mutex<BinaryHeap<TimerEntry>>,
    changed: Condvar,
}

struct TimerEntry {
    due: Instant,
    target: Weak<IsolateTasks>,
}

impl PartialEq for TimerEntry {
    fn eq(&self, other: &Self) -> bool {
        self.due == other.due
    }
}
impl Eq for TimerEntry {}
impl PartialOrd for TimerEntry {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}
impl Ord for TimerEntry {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        other.due.cmp(&self.due)
    }
}

fn timer() -> &'static Timer {
    static TIMER: OnceLock<&'static Timer> = OnceLock::new();
    TIMER.get_or_init(|| {
        let timer: &'static Timer = Box::leak(Box::new(Timer {
            wakes: Mutex::new(BinaryHeap::new()),
            changed: Condvar::new(),
        }));
        std::thread::Builder::new()
            .name("oam-v8-delayed-tasks".to_string())
            .spawn(move || timer.run())
            .expect("spawn the V8 delayed-task timer thread");
        timer
    })
}

impl Timer {
    fn schedule(&self, due: Instant, target: Weak<IsolateTasks>) {
        let mut wakes = lock(&self.wakes);
        let earliest = wakes.peek().is_none_or(|next| due < next.due);
        wakes.push(TimerEntry { due, target });
        if earliest {
            self.changed.notify_one();
        }
    }

    fn run(&self) {
        let mut wakes = lock(&self.wakes);
        loop {
            let now = Instant::now();
            let mut due = Vec::new();
            while wakes.peek().is_some_and(|next| next.due <= now) {
                if let Some(entry) = wakes.pop() {
                    due.push(entry.target);
                }
            }
            if !due.is_empty() {
                drop(wakes);
                for target in due {
                    if let Some(tasks) = target.upgrade() {
                        tasks.promote_due(now);
                    }
                }
                wakes = lock(&self.wakes);
                continue;
            }
            wakes = match wakes.peek().map(|next| next.due) {
                Some(next) => {
                    self.changed
                        .wait_timeout(wakes, next.saturating_duration_since(now))
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .0
                }
                None => self
                    .changed
                    .wait(wakes)
                    .unwrap_or_else(|poisoned| poisoned.into_inner()),
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Odd keys: a real isolate's address is aligned, so these never collide
    // with an isolate another test in this process is running.
    fn entry(key: IsolateKey) -> Option<Arc<IsolateTasks>> {
        lock(registry()).get(&key).cloned()
    }

    /// The freed address of a disposed isolate is reused before its
    /// Registration drops: the new isolate must get a live queue, and the old
    /// Registration must leave it mapped.
    #[test]
    fn a_reused_address_never_inherits_a_closed_queue() {
        let key: IsolateKey = 0x0bad_0001;
        let old = Registration {
            key,
            tasks: queue_for(key, true),
        };
        old.close();

        // The dead isolate's own disposal-time posts still hit its tombstone.
        let late = queue_for(key, false);
        assert!(Arc::ptr_eq(&late, &old.tasks));
        assert!(late.is_closed());

        // The new isolate's thread, inside Isolate::new, gets a fresh queue...
        CREATING_ISOLATE.with(|flag| flag.set(true));
        let early = tasks_for(key);
        CREATING_ISOLATE.with(|flag| flag.set(false));
        assert!(!early.is_closed());
        assert!(!Arc::ptr_eq(&early, &old.tasks));

        // ...and register() hands it that same queue.
        let new_tasks = queue_for(key, true);
        assert!(Arc::ptr_eq(&new_tasks, &early));

        drop(old);
        let mapped = entry(key).expect("the new isolate's queue stays mapped");
        assert!(Arc::ptr_eq(&mapped, &new_tasks));

        drop(Registration {
            key,
            tasks: new_tasks,
        });
        assert!(entry(key).is_none());
    }

    /// register() alone (no post during Isolate::new) also replaces the
    /// tombstone.
    #[test]
    fn register_replaces_a_tombstone_without_an_early_post() {
        let key: IsolateKey = 0x0bad_0003;
        let old = Registration {
            key,
            tasks: queue_for(key, true),
        };
        old.close();
        let new_tasks = queue_for(key, true);
        assert!(!new_tasks.is_closed());
        assert!(!Arc::ptr_eq(&new_tasks, &old.tasks));
        drop(old);
        assert!(entry(key).is_some_and(|tasks| Arc::ptr_eq(&tasks, &new_tasks)));
        drop(Registration {
            key,
            tasks: new_tasks,
        });
        assert!(entry(key).is_none());
    }

    /// Outside isolate creation, a live entry is shared, not replaced.
    #[test]
    fn a_live_queue_is_reused() {
        let key: IsolateKey = 0x0bad_0005;
        let first = queue_for(key, false);
        assert!(Arc::ptr_eq(&first, &tasks_for(key)));
        assert!(Arc::ptr_eq(&first, &queue_for(key, true)));
        drop(Registration { key, tasks: first });
        assert!(entry(key).is_none());
    }
}
