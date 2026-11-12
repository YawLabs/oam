//! N-API (Node-API) alpha: the C-ABI layer compiled .node addons call.
//!
//! Scope of the alpha — the value/property/function/error core
//! (create/read numbers, strings, booleans, objects, arrays;
//! named/indexed properties; native-backed functions with callbacks;
//! throw/catch), plus references, napi_wrap / napi_define_class,
//! bigint, buffers / arraybuffers / typed arrays / dataviews, promises
//! and dates. NOT real yet: the async surfaces -- napi_*_async_work and
//! the threadsafe-function family are exported but are stubs (see the
//! "N-API async surfaces" entry in docs/node-divergences.md).
//!
//! ABI model (the Deno approach): `napi_value` IS a `v8::Local<Value>`
//! transmuted to a pointer — `Local` is repr(C) over `NonNull`, asserted
//! below. Every napi call happens while the engine is on the stack
//! (module init or a callback trampoline), so a live `PinScope` exists;
//! the env carries a raw pointer to it, saved/restored re-entrantly at
//! every native entry. Exceptions are DEFERRED: napi_throw_* stores the
//! value in the env, and the trampoline re-throws into V8 on exit.
//!
//! Symbol export: oam_cli/build.rs exports every `napi_*` below from the
//! oam EXECUTABLE (.def on Windows, --export-dynamic on Linux; macOS
//! exes export by default), which is how addons resolve the ABI at load.

use std::ffi::{c_char, c_void};

// napi_value == Local<Value> (pointer-sized, non-null). Compile-time proof.
const _: () = assert!(
    std::mem::size_of::<v8::Local<'static, v8::Value>>() == std::mem::size_of::<*mut c_void>()
);

pub type NapiValue = *mut c_void;
pub type NapiStatus = i32;

pub const NAPI_OK: NapiStatus = 0;
pub const NAPI_INVALID_ARG: NapiStatus = 1;
pub const NAPI_OBJECT_EXPECTED: NapiStatus = 2;
pub const NAPI_STRING_EXPECTED: NapiStatus = 3;
pub const NAPI_FUNCTION_EXPECTED: NapiStatus = 5;
pub const NAPI_NUMBER_EXPECTED: NapiStatus = 6;
pub const NAPI_BOOLEAN_EXPECTED: NapiStatus = 7;
pub const NAPI_ARRAY_EXPECTED: NapiStatus = 8;
pub const NAPI_GENERIC_FAILURE: NapiStatus = 9;
pub const NAPI_PENDING_EXCEPTION: NapiStatus = 10;

/// A persistent reference to a V8 value. Lives in a [`NapiRefSlot`] of
/// `NapiEnv::refs`.
struct NapiRefEntry {
    value: NapiRefValue,
    refcount: u32,
}

/// How a reference holds its value. Node's rule: a reference with a count of
/// zero is WEAK -- it does not keep an object alive, and reads back as "no
/// value" once the object has been collected -- and one with a positive count
/// is strong. Only objects can be held weakly; a primitive stays `Strong` at
/// any count.
enum NapiRefValue {
    Strong(v8::Global<v8::Value>),
    Weak(v8::Weak<v8::Value>),
}

/// `napi_finalize`: `void (*)(napi_env, void* data, void* hint)`.
type NapiFinalize = unsafe extern "C" fn(Env, *mut c_void, *mut c_void);

/// `void (*fun)(void* arg)`: a `napi_add_env_cleanup_hook` callback.
type NapiCleanupHook = unsafe extern "C" fn(*mut c_void);

/// One registered cleanup hook: the `(fun, arg)` pair that identifies it and
/// its place in the teardown order (see [`next_cleanup_seq`]).
struct CleanupHook {
    seq: u64,
    fun: NapiCleanupHook,
    arg: *mut c_void,
}

/// The next position in the teardown order. [`teardown`] runs what holds the
/// HIGHEST position first, which is Node's rule: cleanup hooks run in reverse
/// order of registration. An env takes a position when it is created, for
/// the release of its own references and finalizers, exactly as Node
/// registers a `napi_env`'s own teardown as a cleanup hook when it creates
/// the env -- so every hook an addon registers runs before that addon's
/// remaining finalizers (measured on node v22.22.2: hooks, then finalizers).
/// One counter for the process is enough: only positions inside one runtime
/// are ever compared.
fn next_cleanup_seq() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// One slot of an env's reference table.
///
/// A slot is never removed once created. `napi_delete_reference` takes the
/// entry OUT and bumps `generation`; the index then goes on the free list for
/// the next create. Because a handle carries the generation it was minted
/// with, a handle for a recycled slot fails the generation compare instead of
/// silently resolving to whoever owns the slot now. See the encoding notes
/// above [`encode_ref_handle`].
struct NapiRefSlot {
    /// `None` while the slot is free.
    entry: Option<NapiRefEntry>,
    /// Bumped on every free. Starts at 1 and never reaches 0 again, which is
    /// what keeps a minted handle from ever being all-zero -- the ABI's null
    /// `napi_ref` sentinel.
    generation: usize,
}

/// A JS object the env tracks WITHOUT keeping it alive, plus the native
/// pointer and finalizer that belong to it. Stored in `NapiEnv::tracked`, for
/// `napi_wrap` / `napi_unwrap` and for the external buffers.
///
/// `object` is weak so the entry never pins the object. Its V8 finalizer
/// (see [`NapiEnv::track_weak`]) does one thing when the object is collected:
/// it records the entry's id in `NapiEnv::collected`. It never touches the
/// env. The addon's `finalize` then runs from [`run_pending_finalizers`],
/// outside the garbage collector, which takes the entry out of `tracked` by
/// that id. Dropping the entry drops the `Weak`, which unregisters that V8
/// finalizer -- so an entry removed by `napi_remove_wrap`, or dropped with
/// its env, never fires.
struct NapiWrapEntry {
    object: v8::Weak<v8::Value>,
    native: *mut c_void,
    finalize: Option<NapiFinalize>,
    hint: *mut c_void,
}

/// One env per loaded addon (Node's model), owned by the [`AddonRegistry`]
/// held in an isolate slot.
///
/// An env has two lives. While its runtime runs it is LIVE: a native entry
/// installs a scope on it and the entry points work. [`teardown`] ends that
/// -- cleanup hooks, finalizers, then `closed` -- and from there on it is a
/// TOMBSTONE: still valid memory, but every entry point that would touch V8
/// finds no scope and answers `napi_invalid_arg`, and the few that only
/// touch the env's own tables find them empty. The registry frees a
/// tombstone only once the addon library that was handed its address is
/// verifiably unmapped, and never otherwise (see [`AddonRegistry`]).
pub struct NapiEnv {
    /// `*mut v8::PinScope<'_, '_>` while inside a native entry; null
    /// outside. Saved/restored re-entrantly by the trampolines.
    scope: *mut c_void,
    /// Deferred exception, thrown into V8 when the native frame returns.
    pending: Option<v8::Global<v8::Value>>,
    /// The `FnData` record of every function built by [`create_function`]
    /// (`napi_create_function`, `napi_define_class`, the property
    /// descriptors) whose `External` V8 has not reported collected, keyed by
    /// the `tracked` id that `External` is filed under. `Box` is required
    /// for pointer stability: the `External` carries a raw `*mut FnData`,
    /// and the table moves its values when it grows. A record leaves in
    /// exactly two ways: [`NapiEnv::take_collected`] removes it once its id
    /// comes out of the `collected` queue, and whatever is left drops with
    /// the env. See [`FnData`] for why neither can pull a record out from
    /// under a call.
    fn_data: std::collections::HashMap<usize, Box<FnData>>,
    /// Persistent references (napi_create_reference), addressed by an
    /// index+generation handle rather than by address. No `Box` here, unlike
    /// the two vecs either side: nothing outside this file ever holds a
    /// pointer INTO this table, so its elements are free to move on realloc.
    refs: Vec<NapiRefSlot>,
    /// Indices of `refs` slots whose entry has been deleted and which the next
    /// `napi_create_reference` may reuse.
    ref_free: Vec<usize>,
    /// Folded into every handle this env mints, so a handle from one env
    /// decodes to a miss in another. See [`encode_ref_handle`].
    ref_tag: usize,
    /// Live weakly-tracked objects (napi_wrap entries, external buffers and
    /// the `External` behind every addon function, whose entry carries no
    /// native pointer or finalizer: its payload is the `fn_data` record under
    /// the same id), keyed by the id minted from `next_tracked_id`. A wrapped object carries
    /// its id under a private symbol, so `napi_unwrap` is one property read
    /// and one map lookup rather than a scan.
    tracked: std::collections::HashMap<usize, NapiWrapEntry>,
    /// Next key for `tracked`. Never reused, so a stale id is always a miss.
    next_tracked_id: usize,
    /// Ids of `tracked` entries whose object V8 has collected and whose addon
    /// finalizer has not run yet. Written by the V8 finalizers, which hold a
    /// clone of this `Rc` and nothing else of the env: the garbage collector
    /// therefore never reads or writes a `NapiEnv`, and a reference into one
    /// may be held across a V8 call that allocates. (What must still not be
    /// held across is a call that runs JS or addon code: those re-enter this
    /// ABI through the raw env pointer.) Drained by
    /// [`NapiEnv::take_collected`]; no borrow of the cell outlives the
    /// statement that takes it.
    collected: std::rc::Rc<std::cell::RefCell<Vec<usize>>>,
    /// Backing store for `napi_get_last_error_info`. Node keeps this per-env
    /// too, which is what makes returning a pointer to it sound: the struct
    /// lives as long as the env the addon is calling through.
    last_error: NapiExtendedErrorInfo,
    /// The hooks `napi_add_env_cleanup_hook` registered and nothing has
    /// removed or run yet, in registration order (so ascending `seq`).
    cleanup_hooks: Vec<CleanupHook>,
    /// This env's own position in the teardown order: where the release of
    /// its references and finalizers ([`release_env`]) falls among the
    /// cleanup hooks. See [`next_cleanup_seq`].
    release_seq: u64,
    /// [`teardown`] has run [`release_env`] for this env.
    released: bool,
    /// The env is a tombstone: its runtime is gone (or the process is
    /// exiting), no scope will ever be installed on it again, and a cleanup
    /// hook registered now would never run.
    closed: bool,
}

impl NapiEnv {
    fn new() -> Box<NapiEnv> {
        Box::new(NapiEnv {
            scope: std::ptr::null_mut(),
            pending: None,
            fn_data: std::collections::HashMap::new(),
            refs: Vec::new(),
            ref_free: Vec::new(),
            ref_tag: next_env_ref_tag(),
            tracked: std::collections::HashMap::new(),
            next_tracked_id: 1,
            collected: std::rc::Rc::default(),
            last_error: NapiExtendedErrorInfo {
                error_message: std::ptr::null(),
                engine_reserved: std::ptr::null_mut(),
                engine_error_code: 0,
                error_code: NAPI_OK,
            },
            cleanup_hooks: Vec::new(),
            release_seq: next_cleanup_seq(),
            released: false,
            closed: false,
        })
    }

    /// Give up everything this env owns and leave a closed tombstone behind:
    /// the function records, the tables, the hooks, the pending exception.
    /// What is left is the struct itself, which the entry points can still
    /// be handed safely.
    ///
    /// Called once no JS of the env's isolate can run again -- from the
    /// registry as it drops, and from `Drop` -- so no trampoline can still
    /// reach a function record through a V8 `External`.
    ///
    /// Weak handles are released by `release_env`, which `teardown` (and
    /// `with_test_env`) runs while the isolate is still alive, so the two
    /// loops below normally see nothing. Anything left means the runtime was
    /// dropped without that step having completed, and this runs after
    /// rusty_v8 has marked the isolate disposed and before V8's own
    /// teardown. A `v8::Weak` dropped in that window frees its callback
    /// record without resetting the V8 handle that still points at it, so
    /// the leftovers are leaked rather than dropped: a bounded leak at
    /// isolate teardown instead of a record V8 could still reach.
    ///
    /// Their V8 finalizers never run, but they are NOT gone yet at this
    /// point. In `OwnedIsolate::drop` (v8 150.0.0) the slots -- and so the
    /// registry -- drop first (`prepare_annex_for_dispose`), and the
    /// isolate's finalizer map is drained right after
    /// (`run_remaining_guaranteed_finalizers`), which drops a regular
    /// finalizer uncalled. So each closure registered by `track_weak`, with
    /// its clone of the `collected` queue, briefly OUTLIVES this call. That
    /// is harmless only because the queue is an `Rc` the closure co-owns and
    /// the closure captures nothing else of the env: dropping it later frees
    /// the queue, and touches no freed memory.
    ///
    /// The reference table keeps its slots (each one retired, see
    /// [`NapiEnv::retire_all_refs`]), so a handle the addon still holds stays
    /// a miss for good.
    fn strip(&mut self) {
        for (_, entry) in self.tracked.drain() {
            std::mem::forget(entry.object);
        }
        for entry in self.retire_all_refs() {
            if let NapiRefValue::Weak(weak) = entry.value {
                std::mem::forget(weak);
            }
        }
        self.scope = std::ptr::null_mut();
        self.closed = true;
        self.pending = None;
        self.fn_data = std::collections::HashMap::new();
        self.tracked = std::collections::HashMap::new();
        self.cleanup_hooks = Vec::new();
        *self.collected.borrow_mut() = Vec::new();
    }

    /// What [`teardown`] still has to do for this env, as the position of
    /// its most urgent step and whether that step is a cleanup hook (the
    /// newest one registered) or the env's own release. `None` when nothing
    /// is left.
    ///
    /// The release is owed until it has run once, and again whenever a
    /// later step left something behind for it: a hook or a finalizer that
    /// wrapped another object or created a reference.
    fn next_teardown_step(&self) -> Option<(u64, bool)> {
        let hook = self.cleanup_hooks.last().map(|hook| (hook.seq, true));
        let owes_release = !self.released
            || !self.tracked.is_empty()
            || self.refs.iter().any(|slot| slot.entry.is_some());
        let release = owes_release.then_some((self.release_seq, false));
        hook.max(release)
    }

    /// Where in `cleanup_hooks` the hook registered for exactly this `fun`
    /// and `arg` is, if it is still registered.
    fn cleanup_hook_position(&self, fun: NapiCleanupHook, arg: *mut c_void) -> Option<usize> {
        self.cleanup_hooks
            .iter()
            .position(|hook| std::ptr::fn_addr_eq(hook.fun, fun) && hook.arg == arg)
    }

    /// Record `status` as this env's last error, with `message` as its
    /// reason (none for `napi_ok`), and return it. The safe counterpart of
    /// [`fail`] / [`ok`] for an entry point that already holds the env.
    fn status(
        &mut self,
        status: NapiStatus,
        message: Option<&'static std::ffi::CStr>,
    ) -> NapiStatus {
        self.last_error.error_code = status;
        self.last_error.error_message = message.map_or(std::ptr::null(), |text| text.as_ptr());
        status
    }
}

/// One loaded addon: the env it was handed and the library it runs from.
struct Addon {
    /// `Box` is required: the C ABI dispenses `*mut NapiEnv`, and the
    /// registry's `Vec` moves its elements when it grows. The pointer is
    /// derived from this box IN PLACE (`load_addon`, `teardown`,
    /// `Drop for AddonRegistry`) and the box is never moved out of its
    /// element, because moving a `Box` invalidates every pointer derived
    /// from it (`oam_aliasing_model::held_derive_then_move_box_is_ub`) and
    /// the addon keeps its copy for as long as it likes. `None` only once
    /// the registry has freed the env, as it drops.
    env: Option<Box<NapiEnv>>,
    /// `None` only once the registry has unloaded it, as it drops.
    library: Option<libloading::Library>,
    /// An address inside the library's image (its `napi_register_module_v1`),
    /// kept as a number: it is only ever handed to
    /// `oam_core::address_in_loaded_image`, never called or read through.
    anchor: usize,
}

/// Per-runtime owner of what N-API addons were given: one `NapiEnv` and one
/// `libloading::Library` handle per `.node` file that was opened and found
/// to be an addon.
///
/// The registry is not a `JsRuntime` field: it lives in an isolate slot
/// (`isolate.set_slot` in `JsRuntime::new_inner`, read back with
/// `get_slot_mut::<AddonRegistry>()` in `load_addon`).
///
/// # The order an addon is torn down in
///
/// Three things must hold, whatever the addon does: a finalizer runs exactly
/// once and before its library is unloaded; no addon code runs after its
/// library is unmapped; and no env is freed while addon code that holds its
/// address can still run. Addon code that runs late is the hard case: a C++
/// static destructor, a `Drop` in a Rust static, `DllMain` on detach, an ELF
/// `.fini` -- all run INSIDE the unload, or at process exit, and any of them
/// may call the N-API ABI with the `napi_env` it saved at registration.
///
/// 1. `JsRuntime::drop` calls [`teardown`] while the isolate is alive. It
///    runs the cleanup hooks (`napi_add_env_cleanup_hook`) in reverse order
///    of registration and, for each env at its place in that order, the
///    finalizers the env still owes, with a scope installed -- Node's order,
///    measured on v22.22.2 (hooks, then finalizers). Every env is then
///    marked closed: a tombstone. From the moment it starts, loading an
///    addon is refused, so no hook or finalizer can bring in an env it would
///    not see.
/// 2. The registry drops with the isolate's slots: rusty_v8 drops them as
///    the `OwnedIsolate` drops, after the isolate has been exited and before
///    V8's own teardown (v8 150.0.0, `prepare_annex_for_dispose`).  No JS
///    runs past that point, and `napi_trampoline` -- the only reader of the
///    `*mut FnData` a V8 `External` carries -- needs running JS, so every
///    env is stripped there ([`NapiEnv::strip`]): the Externals whose record
///    was still in an env dangle through V8's teardown and are never read
///    again. Anything that later attaches a GC finalizer reaching back into
///    an env has to respect this: by V8's teardown the env is an empty
///    tombstone.
/// 3. THE TOP-LEVEL RUNTIME STOPS THERE. Its libraries are never unloaded
///    and its tombstones are never freed, which is what Node does for
///    addons loaded on the main thread: it never calls `dlclose` on them.
///    Late addon code then runs at process exit, against a tombstone, and
///    is answered with a status. (Unloading would also pull the code out
///    from under any thread the addon started.) The cost is one stripped
///    `NapiEnv` and one library reference per addon load, per top-level
///    runtime, for the life of the process.
/// 4. A WORKER'S REGISTRY (`unload`; a `worker_threads` worker, an
///    `oam.fork()` isolate) unloads its libraries next, newest first, as
///    Node closes a worker's addons once its environment is cleaned up. The
///    tombstones are all still allocated while the unloads run, so a
///    destructor an unload runs gets a status, not a dangling env.
/// 5. Only then is a worker's env freed, and only if
///    `oam_core::address_in_loaded_image` says the library's image is no
///    longer mapped -- addon code that is gone cannot use the pointer. An
///    unload often unmaps nothing: the same `.node` is loaded on the main
///    thread too, or the platform keeps the image (by their documentation,
///    not measured here: musl never unloads; macOS keeps an image with
///    thread-local variables; glibc keeps one with a pending thread-local
///    destructor or `NODELETE`). That env stays a tombstone for the life of
///    the process, like the top-level runtime's. Measured with the test
///    addon as a worker's only loader: Windows and Linux (glibc 2.39) unmap
///    it, and the env is freed.
///
/// A hard exit (`process.exit()`, a fatal error) drops no runtime: nothing
/// is unloaded or freed. [`close_for_exit`] closes the envs of the isolate
/// that called `process.exit()`, so the destructors the C runtime then runs
/// are refused instead of entering a V8 that is being abandoned. Cleanup
/// hooks and finalizers do not run on that path, as they do not in Node
/// (measured on v22.22.2).
///
/// # What remains outside this
///
/// * A `napi_value` is valid only inside the native call it was produced
///   in, in Node as here. Late addon code that passes one it kept is passing
///   a pointer into a heap that no longer exists; the entry points that
///   inspect a value before they look for a scope cannot detect that.
/// * A tombstone is not synchronized. Late addon code on one thread that
///   uses the env of a runtime still running on ANOTHER thread (a worker
///   alive while the process exits) races that worker, as it does in Node.
/// * The mapped check sees the addon's own image. An addon that hands its
///   `napi_env` to a different library, which outlives it, is not seen.
/// * A hard exit other than `process.exit()` (the fatal-error banner, the
///   EPIPE bail) taken while an addon's native call is on the stack leaves
///   that env open: late addon code is then served by the live isolate, as
///   in Node, rather than refused.
pub struct AddonRegistry {
    /// In load order.
    addons: Vec<Addon>,
    /// Whether this registry unloads its libraries when it drops: true for
    /// a worker's or a fork's runtime, false for a top-level one.
    unload: bool,
    /// [`teardown`] has begun: no addon may be loaded any more. A hook or a
    /// finalizer that teardown runs may still run JS, and that JS a
    /// `require()` of a `.node` -- whose env would arrive after teardown
    /// took its list, and so never get its hooks or finalizers run before
    /// its library was unloaded. Node gives a cleanup hook no handle scope:
    /// calling into JS from one aborts it (measured on v22.22.2).
    closing: bool,
}

impl AddonRegistry {
    /// `unload`: whether the runtime this registry belongs to gives its
    /// addon libraries back when it ends (a worker's does, a top-level
    /// runtime's never does -- see the type's docs).
    pub fn new(unload: bool) -> Self {
        Self {
            addons: Vec::new(),
            unload,
            closing: false,
        }
    }

    /// The env of every addon, as the pointer the addon itself holds.
    /// Derived from each box in place; see [`Addon::env`].
    fn envs(&mut self) -> Vec<Env> {
        self.addons
            .iter_mut()
            .filter_map(|addon| addon.env.as_mut())
            .map(|env| &raw mut **env)
            .collect()
    }
}

impl Drop for AddonRegistry {
    fn drop(&mut self) {
        for env in self.envs() {
            // SAFETY: `env` was just derived from a box this registry owns,
            // and no other reference into it exists: the isolate is no
            // longer entered on this thread (its slots drop after that), so
            // no native entry is on the stack. For the same reason no JS can
            // run again, which is what `strip` needs to free the function
            // records.
            unsafe { (*env).strip() };
        }
        if self.unload {
            // Every env is a tombstone and still allocated: whatever a
            // library's unload runs may call the ABI with one.
            for addon in self.addons.iter_mut().rev() {
                drop(addon.library.take());
            }
            for addon in &mut self.addons {
                if !oam_core::address_in_loaded_image(addon.anchor) {
                    // The image is gone, and its code with it: nothing can
                    // present this env again. Dropped in place.
                    addon.env = None;
                }
            }
        }
        // What is left -- every library handle and env of a registry that
        // does not unload, and the envs whose library is still mapped -- has
        // to outlive this runtime. Forgetting the `Vec` leaks its elements
        // where they are, without moving a box.
        #[cfg(test)]
        NAPI_ENV_KEPT_COUNT.with(|c| {
            let kept = self.addons.iter().filter(|addon| addon.env.is_some());
            c.set(c.get() + kept.count());
        });
        std::mem::forget(std::mem::take(&mut self.addons));
    }
}

// Thread-local counters: each `NapiEnv::drop` increments the first, and a
// registry increments the second for each env it leaves allocated as a
// tombstone when it drops. Used only in tests, to verify that every env a
// runtime handed out is accounted for one way or the other.
// Not a doc comment -- rustdoc can't document items produced by a macro.
#[cfg(test)]
thread_local! {
    pub static NAPI_ENV_DROP_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub static NAPI_ENV_KEPT_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl Drop for NapiEnv {
    fn drop(&mut self) {
        #[cfg(test)]
        NAPI_ENV_DROP_COUNT.with(|c| c.set(c.get() + 1));
        // Normally a no-op: the registry strips an env before it decides
        // whether to free it. `with_test_env`'s env comes straight here.
        self.strip();
    }
}

/// Release everything `env` holds weakly, and run the addon finalizers it
/// still owes, while the isolate is alive: the wrap / external-buffer entries
/// (live ones and those whose object is already collected) and then the
/// whole reference table.
/// Node does the same at environment teardown -- remaining finalizers run,
/// then the references go.
///
/// The `fn_data` records are NOT released here, although the weak handles
/// that would have reported their functions collected are: the finalizers
/// below run with a scope installed and may still call an addon function,
/// and so may whatever JS runs between this and the env dropping. The
/// records go with the env, once -- they are plain memory with no addon
/// finalizer attached, so nothing is owed for them here.
///
/// The finalizers run with whatever native scope the caller left installed
/// on `env`. [`teardown`] installs one, as Node does at environment
/// teardown, so a finalizer may still read a reference's value or open a
/// handle scope there. With none installed (`with_test_env`) a finalizer
/// that asks for a JS value gets `napi_invalid_arg`; releasing native memory
/// and `napi_delete_reference` work either way.
///
/// # Safety
///
/// `env` must be a live `*mut NapiEnv` of an isolate that has not been
/// disposed, with no reference into it held by the caller, and the addon
/// library its finalizers live in must still be loaded.
unsafe fn release_env(env: Env) {
    let tracked = {
        // SAFETY: `env` is live per this function's contract, and the caller
        // holds no reference into it; this reborrow ends with the block,
        // before anything below calls into the addon.
        let env_ref = unsafe { &mut *env };
        // Every entry is released below, collected or not, so the ids V8 has
        // reported so far name nothing that is still owed.
        env_ref.collected.borrow_mut().clear();
        std::mem::take(&mut env_ref.tracked)
    };
    let mut owed = Vec::new();
    for entry in tracked.into_values() {
        let NapiWrapEntry {
            object,
            native,
            finalize,
            hint,
        } = entry;
        // Dropped with the isolate alive, so the V8 handle is reset and its
        // finalizer unregistered before the addon's callback can re-enter.
        drop(object);
        if let Some(finalize) = finalize {
            owed.push((finalize, native, hint));
        }
    }
    for (finalize, native, hint) in owed {
        // SAFETY: `finalize` is the `napi_finalize` the addon registered for
        // `native` / `hint`, called once (its entry was consumed above) with
        // the env it was registered on; the library is loaded per contract.
        unsafe { finalize(env, native, hint) };
    }
    let refs = {
        // SAFETY: as above -- `env` is live, and no reference into it
        // survived the finalizers above, which re-enter this ABI; this
        // reborrow ends with the block.
        let env_ref = unsafe { &mut *env };
        // Retired one by one, never reset: addon code runs after this (the
        // cleanup hooks registered earlier, another round of this release),
        // and a handle minted before must stay a miss there -- not decode to
        // whatever reference the reset table would mint next under the same
        // index and generation.
        env_ref.retire_all_refs()
    };
    drop(refs);
}

/// Whether `isolate` has any addon env for [`teardown`] to release. Lets
/// `JsRuntime::drop` skip opening a scope for a runtime that never loaded an
/// addon.
pub(crate) fn has_envs(isolate: &v8::Isolate) -> bool {
    isolate
        .get_slot::<AddonRegistry>()
        .is_some_and(|registry| !registry.addons.is_empty())
}

/// End the live part of every addon env of the isolate behind `scope`: run
/// the cleanup hooks and the finalizers still owed, then close the envs.
/// Called from `JsRuntime::drop` while the isolate is alive: once the
/// registry slot itself drops, the isolate is already marked disposed and a
/// `v8::Weak` can no longer be released properly (see [`NapiEnv::strip`]).
///
/// The order is Node's, which runs an environment's cleanup hooks in
/// reverse order of registration and counts the release of each `napi_env`
/// as a hook registered when that env was created (see
/// [`next_cleanup_seq`]): the step holding the highest position runs next,
/// until none is left. For one addon that is its hooks, newest first, and
/// then [`release_env`] -- its remaining finalizers. A step may add work (a
/// hook that registers a hook, a finalizer that wraps another object); the
/// loop picks that up, as Node's does.
///
/// `scope` is installed on every env for the whole of it, the way a native
/// entry installs one: Node runs teardown with a handle scope and context
/// entered, and a hook or a finalizer may rely on that (reading a
/// reference's value on its way to releasing it, for one) through whichever
/// env the addon saved. An exception a step leaves pending is dropped --
/// there is no caller left to throw it to.
///
/// The envs are closed when this returns: tombstones, whose library the
/// registry may then unload (see [`AddonRegistry`]).
pub(crate) fn teardown(scope: &mut v8::PinScope<'_, '_>) {
    let Some(registry) = scope.get_slot_mut::<AddonRegistry>() else {
        return;
    };
    // From here on `load_addon` refuses, so this list is every env there
    // will be: an addon loaded by JS that a step below runs would be one
    // the loop never sees.
    registry.closing = true;
    // The registry borrow ends with this statement.
    let envs = registry.envs();
    let scope_ptr = scope as *mut v8::PinScope<'_, '_> as *mut c_void;
    for &env in &envs {
        // SAFETY: each pointer was just derived from a registry-owned env
        // and no reference into one is held here. `scope_ptr` comes from the
        // caller's exclusive scope, which is not used again until the envs
        // are closed below, where it is cleared from every one of them: it
        // never outlives this frame.
        unsafe { (*env).scope = scope_ptr };
    }
    loop {
        let mut next: Option<(u64, bool, Env)> = None;
        for &env in &envs {
            // SAFETY: `env` is live (above) and nothing else is running: the
            // shared borrow ends with this statement.
            let candidate = unsafe { (*env).next_teardown_step() };
            if let Some((seq, is_hook)) = candidate
                && next.is_none_or(|(best, _, _)| seq > best)
            {
                next = Some((seq, is_hook, env));
            }
        }
        let Some((_, is_hook, env)) = next else {
            break;
        };
        if is_hook {
            // SAFETY: `env` is live; the hook is taken out of the env before
            // it is called, so the borrow has ended when addon code runs and
            // re-enters this ABI. `fun` is the function the addon registered
            // for `arg`, called once (it was just removed), and the registry
            // keeps the addon library loaded until it drops, after this.
            unsafe {
                if let Some(hook) = (*env).cleanup_hooks.pop() {
                    (hook.fun)(hook.arg);
                }
            }
        } else {
            // SAFETY: `env` is live, the isolate is alive (the caller holds
            // its scope), no reference into the env is held across the call,
            // and the library its finalizers live in is still loaded, as
            // above.
            unsafe {
                (*env).released = true;
                release_env(env);
            }
        }
    }
    for &env in &envs {
        // SAFETY: `env` is live and no addon code is running any more, so
        // no reference into it exists.
        let leftover = unsafe {
            (*env).scope = std::ptr::null_mut();
            (*env).closed = true;
            (*env).pending.take()
        };
        // Dropped outside any borrow of the env: it disposes a V8 handle.
        drop(leftover);
    }
}

/// Close every addon env of the isolate behind `scope` because the process
/// is about to exit WITHOUT dropping its runtimes (`process.exit()`).
///
/// Nothing is unloaded or freed on that path, so an env stays valid memory
/// for the destructors the C runtime runs on the way out. What must not
/// happen is one of them getting INTO V8: when `process.exit()` was called
/// from JS that an addon's native call invoked, that call's scope is still
/// installed on the env, and it never returns to clear it. Closing the env
/// makes such a late call a refusal, like any other call outside a native
/// entry. No hook or finalizer runs here, as none does in Node on
/// `process.exit()` (measured on v22.22.2).
pub(crate) fn close_for_exit(scope: &mut v8::PinScope<'_, '_>) {
    let Some(registry) = scope.get_slot_mut::<AddonRegistry>() else {
        return;
    };
    for env in registry.envs() {
        // SAFETY: `env` was just derived from a registry-owned env. The only
        // other pointers to it are raw: the addon's own, and those of native
        // entries further up this stack, which are never returned to -- the
        // caller exits the process.
        unsafe {
            (*env).scope = std::ptr::null_mut();
            (*env).closed = true;
        }
    }
}

// Thread-local counter of entries actually PUSHED into `NapiEnv::refs` by
// napi_create_reference. Used only in tests, to prove a rejected call creates
// nothing: the orphan a failed create used to leave behind is invisible from
// JS (nothing exposes `env.refs`), so this is the seam that makes it testable.
// Not a doc comment -- rustdoc can't document items produced by a macro.
#[cfg(test)]
thread_local! {
    pub static NAPI_REF_PUSH_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

type Env = *mut NapiEnv;

/// Build a `NapiEnv` with `scope` installed, hand it plus one valid
/// `napi_value` to `f`, then tear the scope back down -- the same install /
/// restore a native entry performs, so the C entry points behave exactly as
/// they do under a real addon. Test-only.
#[cfg(test)]
pub(crate) fn with_test_env<R>(
    scope: &mut v8::PinScope<'_, '_>,
    f: impl FnOnce(Env, NapiValue) -> R,
) -> R {
    let mut env = Box::new(NapiEnv::new());
    let value = from_local(v8::Object::new(scope).into());
    // Derived from the EXCLUSIVE borrow, as the trampoline and load_addon do:
    // `env_scope_mut` hands out `&mut PinScope` through this pointer.
    env.scope = std::ptr::from_mut(&mut *scope) as *mut c_void;
    let ptr: Env = &raw mut **env;
    let out = f(ptr, value);
    // Match load_addon: the scope must not outlive the frame that installed it.
    env.scope = std::ptr::null_mut();
    // This env is not in the AddonRegistry, so `teardown` never sees it.
    // Release its weak handles here, while the isolate is alive: a finalizer
    // left registered would otherwise fire into a freed env.
    // Re-derived: the write through `env` just above ended the first `ptr`.
    let ptr: Env = &raw mut **env;
    // SAFETY: `ptr` is this function's own live env; the isolate behind
    // `scope` is alive; no reference into the env is held across the call;
    // and a test registers only finalizers defined in the test binary.
    unsafe { release_env(ptr) };
    out
}

/// SAFETY HELPERS — every napi fn funnels through these.
///
/// # Safety
///
/// The caller must pass the `env` pointer this engine handed the addon; it is
/// dereferenced and its stashed PinScope recovered -- sound only while a
/// native entry (trampoline / load_addon) is on the stack, which is the only
/// time napi fns run.
unsafe fn env_scope<'a>(env: Env) -> Option<&'a v8::PinScope<'static, 'static>> {
    // SAFETY: `env` is the caller's `*mut NapiEnv`; `as_ref` guards null, then the stashed `scope` field is cast back to a SHARED `&PinScope` -- non-null only while a native entry is on the stack.
    unsafe {
        let env = env.as_ref()?;
        (env.scope as *const v8::PinScope<'static, 'static>).as_ref()
    }
}

/// Exclusive counterpart to [`env_scope`], for the few operations V8 only
/// offers through `&mut`: opening a `TryCatch` and creating a `v8::Weak`.
/// An entry point takes one or the other, never both.
///
/// # Safety
///
/// As [`env_scope`], and the result must be the only scope reference in use
/// for as long as it lives: a caller that also took the shared one must not
/// touch it again once this exists. The pointer was stored from the native
/// entry's own `&mut PinScope`, which that entry does not use while addon
/// code is running, so this is the one live borrow of it.
unsafe fn env_scope_mut<'a>(env: Env) -> Option<&'a mut v8::PinScope<'static, 'static>> {
    // SAFETY: `env` is the caller's `*mut NapiEnv`; `as_ref` guards null, then the stashed `scope` field is cast back to the `&mut PinScope` it was stored from -- non-null only while a native entry is on the stack; exclusivity is the caller's obligation above.
    unsafe {
        let env = env.as_ref()?;
        (env.scope as *mut v8::PinScope<'static, 'static>).as_mut()
    }
}

/// Run one V8 operation that can throw, the way Node's `NAPI_PREAMBLE` does:
/// under a `TryCatch`, with a throw moved into the env's deferred-exception
/// slot. That slot is what `napi_is_exception_pending` and
/// `napi_get_and_clear_last_exception` read, and what the native entry
/// re-throws on return -- so an addon can observe, clear or translate an
/// exception a JS callback raised, and one it leaves alone still propagates.
///
/// `op` returns `None` for V8's empty result. The outcomes:
/// * `Ok(value)` -- nothing was thrown.
/// * `Err(NAPI_PENDING_EXCEPTION)` -- `op` threw (now in the env's slot), or
///   an exception was already pending on entry and `op` was not run (Node
///   refuses the same way), or execution is being terminated (handed back to
///   V8 untouched: there is no JS value for an addon to hold).
/// * `Err(NAPI_GENERIC_FAILURE)` -- an empty result with nothing thrown.
/// * `Err(NAPI_INVALID_ARG)` -- no native scope on this env.
///
/// # Safety
///
/// `env` must be the live `napi_env` this engine handed the addon, or null.
/// The caller must not use a scope reference it took from [`env_scope`]
/// after this call (see [`env_scope_mut`]); `op` gets its own.
unsafe fn catching<T>(
    env: Env,
    op: impl FnOnce(&v8::PinScope<'_, '_>) -> Option<T>,
) -> Result<T, NapiStatus> {
    // SAFETY: `env` is the caller's env pointer or null (null-guarded inside); exclusivity of the returned scope is this function's own contract.
    let Some(scope) = (unsafe { env_scope_mut(env) }) else {
        return Err(NAPI_INVALID_ARG);
    };
    // SAFETY: env_scope_mut returned Some, so `env` is non-null and live; a read of its `pending` slot.
    if unsafe { (*env).pending.is_some() } {
        return Err(NAPI_PENDING_EXCEPTION);
    }
    v8::tc_scope!(let tc, scope);
    let outcome = {
        let inner: &v8::PinScope<'_, '_> = tc;
        op(inner)
    };
    if tc.has_caught() {
        if tc.has_terminated() || !tc.can_continue() {
            tc.rethrow();
            return Err(NAPI_PENDING_EXCEPTION);
        }
        let caught = tc.exception().map(|exception| {
            let inner: &v8::PinScope<'_, '_> = tc;
            v8::Global::new(inner, exception)
        });
        if let Some(caught) = caught {
            // SAFETY: `env` is non-null and live (established above); writes its `pending` deferred-exception slot. No reference into the env is held here.
            unsafe { (*env).pending = Some(caught) };
        }
        return Err(NAPI_PENDING_EXCEPTION);
    }
    outcome.ok_or(NAPI_GENERIC_FAILURE)
}

/// [`catching`], for an entry point whose whole result is the one
/// `napi_value` `op` produces: written to `result` on success, with the
/// status passed through otherwise. The missing-scope check `catching` makes
/// stands in for the entry point's own.
///
/// # Safety
///
/// As [`catching`]; and `result` must be null or valid for a write of one
/// `napi_value`.
unsafe fn catching_value(
    env: Env,
    result: *mut NapiValue,
    op: impl FnOnce(&v8::PinScope<'_, '_>) -> Option<NapiValue>,
) -> NapiStatus {
    // SAFETY: `env` and the scope rule are this function's own contract, passed straight through.
    match unsafe { catching(env, op) } {
        // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
        Ok(value) => unsafe { out(result, value) },
        Err(status) => status,
    }
}

/// Finish a property write made under [`catching`]. A setter or proxy trap
/// that threw is `napi_pending_exception`; V8 declining the write without
/// throwing is `napi_generic_failure`, as in Node.
fn finish_set(outcome: Result<bool, NapiStatus>) -> NapiStatus {
    match outcome {
        Ok(true) => NAPI_OK,
        Ok(false) => NAPI_GENERIC_FAILURE,
        Err(status) => status,
    }
}

/// The private symbol under which `env` records the `tracked` id of an
/// object it has wrapped. Per env (keyed by the env's address, unique among
/// the live envs of a process), so two addons wrapping the same object do
/// not see each other's wrap; private, so it is invisible to JS.
fn wrap_key<'s>(env: Env, scope: &v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Private>> {
    let name = v8::String::new(scope, &format!("oam:napi:wrap:{:x}", env.addr()))?;
    Some(v8::Private::for_api(scope, Some(name)))
}

/// The `tracked` id `target` carries under `key`, if it has been wrapped.
fn wrap_id(
    scope: &v8::PinScope<'_, '_>,
    target: v8::Local<v8::Object>,
    key: v8::Local<v8::Private>,
) -> Option<usize> {
    let value = target.get_private(scope, key)?;
    if !value.is_number() {
        return None;
    }
    // Ids are minted by this file as small integers, exact in an f64.
    Some(value.number_value(scope)? as usize)
}

impl NapiEnv {
    /// Start tracking `value` weakly, with the native pointer and finalizer
    /// that belong to it, and return the fresh `tracked` id it was filed
    /// under.
    ///
    /// The V8 finalizer attached here captures the id and a clone of the
    /// `collected` queue -- not the env. rusty_v8 runs it from the weak
    /// handle's SECOND-PASS callback, which V8 may invoke at the end of a
    /// garbage collection (so from inside whatever V8 call triggered it) or
    /// later from a foreground task it posted to the platform. Either way it
    /// is on the isolate's thread, at a point where addon code must not run,
    /// and with no borrow of the `collected` `RefCell` live: the only other
    /// borrows of it are single statements in `take_collected` and
    /// `release_env` that make no V8 call. All the finalizer does is push
    /// the id. That is what lets a caller keep its `&mut NapiEnv` across
    /// this call and the other allocating V8 calls around it.
    fn track_weak(
        &mut self,
        isolate: &mut v8::Isolate,
        value: v8::Local<v8::Value>,
        native: *mut c_void,
        finalize: Option<NapiFinalize>,
        hint: *mut c_void,
    ) -> usize {
        let id = self.mint_tracked_id();
        self.track_weak_as(id, isolate, value, native, finalize, hint);
        id
    }

    /// A fresh `tracked` id. Never reused, so a stale id is always a miss.
    fn mint_tracked_id(&mut self) -> usize {
        let id = self.next_tracked_id;
        self.next_tracked_id = id + 1;
        id
    }

    /// [`NapiEnv::track_weak`] under an id the caller minted beforehand
    /// with [`NapiEnv::mint_tracked_id`]. `create_function` needs the id
    /// before the object exists: the `fn_data` record filed under it is what
    /// the `External` is created around.
    fn track_weak_as(
        &mut self,
        id: usize,
        isolate: &mut v8::Isolate,
        value: v8::Local<v8::Value>,
        native: *mut c_void,
        finalize: Option<NapiFinalize>,
        hint: *mut c_void,
    ) {
        let collected = std::rc::Rc::clone(&self.collected);
        let object = v8::Weak::with_finalizer(
            isolate,
            value,
            Box::new(move |_isolate: &mut v8::Isolate| collected.borrow_mut().push(id)),
        );
        self.tracked.insert(
            id,
            NapiWrapEntry {
                object,
                native,
                finalize,
                hint,
            },
        );
    }

    /// Take the entries whose object V8 has reported collected since the
    /// last call, out of `tracked`. An id whose entry is already gone is
    /// skipped.
    ///
    /// This is also where the `FnData` record of a collected function is
    /// released: an id that names an addon function's `External` has its
    /// record under the same key in `fn_data`, and the record drops here --
    /// at a native entry (`run_pending_finalizers`), never inside the
    /// collector. The `External` was the only path to the record and V8 has
    /// reported it dead, so no call can arrive through it any more; see
    /// [`FnData`].
    fn take_collected(&mut self) -> Vec<NapiWrapEntry> {
        let ids = std::mem::take(&mut *self.collected.borrow_mut());
        ids.into_iter()
            .filter_map(|id| {
                self.fn_data.remove(&id);
                self.tracked.remove(&id)
            })
            .collect()
    }
}

/// Run the addon finalizers owed for objects that have been collected.
/// Called at a native entry, before the addon's callback: outside the
/// garbage collector, with a scope installed, which is where Node-API lets a
/// finalizer use the env.
///
/// # Safety
///
/// `env` must be live with a native scope installed, and the caller must
/// hold no reference into it.
unsafe fn run_pending_finalizers(env: Env) {
    loop {
        // SAFETY: `env` is live per this function's contract; the collected entries are taken out whole, so the borrow ends with the statement and a finalizer that re-enters never observes a live borrow.
        let batch = unsafe { &mut *env }.take_collected();
        if batch.is_empty() {
            return;
        }
        for entry in batch {
            let NapiWrapEntry {
                object,
                native,
                finalize,
                hint,
            } = entry;
            drop(object);
            if let Some(finalize) = finalize {
                // SAFETY: `finalize` is the `napi_finalize` the addon registered for `native` / `hint`, called once (its entry was consumed above) with the env it was registered on.
                unsafe { finalize(env, native, hint) };
            }
        }
    }
}

/// # Safety
///
/// The caller must pass a `napi_value` produced by this engine; it is
/// reinterpreted as a `v8::Local` (ABI-identical, size-asserted). Null yields
/// None.
unsafe fn to_local<'s>(value: NapiValue) -> Option<v8::Local<'s, v8::Value>> {
    if value.is_null() {
        None
    } else {
        // SAFETY: reinterprets the non-null `napi_value` pointer as a `v8::Local<Value>`; the two are ABI-identical (size-asserted at module top).
        Some(unsafe { std::mem::transmute::<NapiValue, v8::Local<'s, v8::Value>>(value) })
    }
}

fn from_local(local: v8::Local<'_, v8::Value>) -> NapiValue {
    // SAFETY: ABI pun of a `v8::Local<Value>` to the pointer-sized `napi_value` (size-asserted equal at module top).
    unsafe { std::mem::transmute::<v8::Local<'_, v8::Value>, NapiValue>(local) }
}

/// Record WHY an entry point is failing, then return that status.
///
/// `napi_get_last_error_info` is the documented way an addon asks "why did
/// that fail?", and returning a permanently-empty struct answered "nothing
/// failed" -- actively misleading, precisely when the caller is debugging.
/// Coverage is NOT uniform: today only the reference family
/// (`napi_create_reference`, `napi_delete_reference`,
/// `napi_get_reference_value`, `napi_reference_ref`,
/// `napi_reference_unref`) goes through `fail` / [`ok`]. Every other entry
/// point returns a bare status and leaves the slot as the last recording
/// call left it, so `napi_get_last_error_info` can describe an older call
/// (docs/node-divergences.md, divergence 23). An entry point that starts
/// recording here must also clear on success via [`ok`], or its own stale
/// reason outlives it.
///
/// # Safety
///
/// `env` must be the live `napi_env` this engine handed the addon, or null.
unsafe fn fail(env: Env, status: NapiStatus, message: &'static std::ffi::CStr) -> NapiStatus {
    // SAFETY: caller guarantees `env` is this engine's env pointer or null;
    // `as_mut` guards null, and the write only touches the env's own slot.
    if let Some(env_ref) = unsafe { env.as_mut() } {
        env_ref.last_error.error_code = status;
        env_ref.last_error.error_message = message.as_ptr();
    }
    status
}

/// Clear the last-error slot and return NAPI_OK.
///
/// # Safety
///
/// `env` must be the live `napi_env` this engine handed the addon, or null.
unsafe fn ok(env: Env) -> NapiStatus {
    // SAFETY: as in `fail` -- null-guarded, writes only the env's own slot.
    if let Some(env_ref) = unsafe { env.as_mut() } {
        env_ref.last_error.error_code = NAPI_OK;
        env_ref.last_error.error_message = std::ptr::null();
    }
    NAPI_OK
}

/// # Safety
///
/// The caller must pass a valid writable `*mut T` (or null) for `ptr`; out()
/// null-checks before writing `value`.
unsafe fn out<T>(ptr: *mut T, value: T) -> NapiStatus {
    if ptr.is_null() {
        return NAPI_INVALID_ARG;
    }
    // SAFETY: `ptr` was null-checked at the top of out(); writes `value` into the caller's out-parameter.
    unsafe { ptr.write(value) };
    NAPI_OK
}

/// Callback registration payload: one record per function built by
/// [`create_function`], owned by `NapiEnv::fn_data` and reached from V8
/// through the `v8::External` that is the function's callback data.
///
/// # Lifetime
///
/// The record is released after its function is collected, as Node's is --
/// an addon that creates a function per call no longer grows the env for as
/// long as it runs. What is tracked (weakly, [`NapiEnv::track_weak_as`]) is
/// the `External`, not the `Function`: the `External` is the only thing
/// that carries the record's address, and [`napi_trampoline`] is its only
/// reader, so the `External`'s death is exactly "no call can reach this
/// record again". Everything that can still call the function keeps the
/// `External` alive through it -- the function itself, a bound function
/// (which holds its target), a class constructor reached from an instance's
/// prototype chain, a prototype method reached from the prototype -- and
/// each of those functions has a record and an `External` of its own, so
/// one dying says nothing about another. Tracking the `Function` instead
/// would rest on V8 never reaching the `External` from anything that
/// outlives it.
///
/// The steps, none of which run in the collector:
///
/// 1. V8 collects the `External`; its finalizer pushes the record's id onto
///    `NapiEnv::collected` and touches nothing else.
/// 2. The env's next native entry (`run_pending_finalizers`) drains the
///    queue through [`NapiEnv::take_collected`], which drops the record.
/// 3. A record whose `External` was never reported collected drops with the
///    env. `release_env` deliberately leaves the records in place (addon
///    finalizers it runs may still call a function), so past it nothing is
///    released early any more.
///
/// A record cannot go away under a call made through it, for two
/// independent reasons. V8 hands the trampoline the `External` as a live
/// handle of the callback's own scope, so it is not collected -- and its id
/// not queued -- before the callback has returned, whatever the callback
/// drops and however many collections and nested native entries it causes.
/// And the trampoline copies the three fields out before it runs anything,
/// so nothing reads the record again after that first statement.
///
/// A record whose Function failed to build is released at once.
struct FnData {
    cb: unsafe extern "C" fn(Env, *mut CbInfo) -> NapiValue,
    data: *mut c_void,
    env: Env,
}

// Thread-local counters of `FnData` records FILED in an env by
// `create_function` and of records actually FREED. Used only in tests: a
// record is invisible from JS, so this is the seam that shows WHEN one is
// released and that it is released once.
// Not a doc comment -- rustdoc can't document items produced by a macro.
#[cfg(test)]
thread_local! {
    pub static NAPI_FN_DATA_FILED_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub static NAPI_FN_DATA_FREED_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
impl Drop for FnData {
    fn drop(&mut self) {
        NAPI_FN_DATA_FREED_COUNT.with(|c| c.set(c.get() + 1));
    }
}

/// What napi_get_cb_info reads. Lives on the trampoline's stack.
#[repr(C)]
pub struct CbInfo {
    args: Vec<NapiValue>,
    this: NapiValue,
    data: *mut c_void,
    /// `new.target` of a construct call; null for a plain call, which is how
    /// Node-API reports "not called with `new`" (napi_get_new_target).
    new_target: NapiValue,
}

/// The zero-capture trampoline behind every napi_create_function.
fn napi_trampoline(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Ok(external) = v8::Local::<v8::External>::try_from(args.data()) else {
        return;
    };
    // SAFETY: the External's payload is the `*mut FnData` stored when this function was built (`create_function`). The record is owned by `NapiEnv::fn_data` and leaves it only for an id V8 reported collected -- that is, after this External died -- or with the env; V8 has just handed this External over alive, so the record is live. The shared reference ends with this statement: the fields are copied out and the record is not read again, so nothing below (the finalizers, the addon callback, a nested entry) can observe it going away.
    let (cb, data, env) = unsafe {
        let fn_data = &*(external.value() as *const FnData);
        (fn_data.cb, fn_data.data, fn_data.env)
    };

    let mut collected = Vec::with_capacity(args.length() as usize);
    for i in 0..args.length() {
        collected.push(from_local(args.get(i)));
    }
    let mut info = CbInfo {
        args: collected,
        this: from_local(args.this().into()),
        data,
        new_target: if args.is_construct_call() {
            from_local(args.new_target())
        } else {
            std::ptr::null_mut()
        },
    };

    // Re-entrant scope swap: nested napi_call_function arrives back here.
    // SAFETY: `env` is the FnData's owning env; swaps in the current PinScope, runs the finalizers owed for collected objects (no reference into the env is held here; this is also where the records of collected functions are released, never this call's own -- its External is alive), then invokes the addon callback `cb` (a C-ABI fn pointer from the addon, copied out of the record above), and returns the previous scope pointer to restore re-entrantly. A finalizer that left an exception pending skips the callback: the exception is thrown below instead of being attributed to a callback that then could not call into JS.
    let (prev_scope, result) = unsafe {
        let prev = (*env).scope;
        (*env).scope = scope as *mut v8::PinScope<'_, '_> as *mut c_void;
        run_pending_finalizers(env);
        let result = if (*env).pending.is_some() {
            std::ptr::null_mut()
        } else {
            cb(env, &mut info as *mut CbInfo)
        };
        (prev, result)
    };
    // SAFETY: `env` is the FnData's owning env; restores the previous scope pointer and, if the callback left a deferred exception, re-throws it into V8.
    unsafe {
        (*env).scope = prev_scope;
        if let Some(pending) = (*env).pending.take() {
            let exception = v8::Local::new(scope, &pending);
            scope.throw_exception(exception);
            return;
        }
    }
    // SAFETY: `result` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    if let Some(local) = unsafe { to_local(result) } {
        rv.set(local);
    }
}

// =========================================================== value creation

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_undefined(env: Env, result: *mut NapiValue) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(v8::undefined(scope).into())) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_null(env: Env, result: *mut NapiValue) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(v8::null(scope).into())) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_global(env: Env, result: *mut NapiValue) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let context = scope.get_current_context();
    let global = context.global(scope);
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(global.into())) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_boolean(
    env: Env,
    value: bool,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(v8::Boolean::new(scope, value).into())) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_int32(
    env: Env,
    value: i32,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(v8::Integer::new(scope, value).into())) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_uint32(
    env: Env,
    value: u32,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe {
        out(
            result,
            from_local(v8::Integer::new_from_unsigned(scope, value).into()),
        )
    }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_int64(
    env: Env,
    value: i64,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // Always a Number, as in Node (`Number::New(isolate,
    // static_cast<double>(value))`): past +/-(2^53 - 1) the value is rounded
    // to the nearest double, which the Node-API docs call out as a loss of
    // precision. The BigInt entry point is napi_create_bigint_int64; an
    // addon that picked this one gets `typeof === "number"` at every
    // magnitude.
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe {
        out(
            result,
            from_local(v8::Number::new(scope, value as f64).into()),
        )
    }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_double(
    env: Env,
    value: f64,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(v8::Number::new(scope, value).into())) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_string_utf8(
    env: Env,
    string: *const c_char,
    length: usize,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    if !is_napi_string_args(string.cast(), length, result) {
        return NAPI_INVALID_ARG;
    }
    // SAFETY: `string` is null only with length 0 (checked above), and otherwise the caller's text of `length` bytes, NUL-terminated when the length is NAPI_AUTO_LENGTH.
    let bytes = unsafe { napi_text_units(string.cast::<u8>(), length) };
    let text = String::from_utf8_lossy(bytes);
    let Some(created) = v8::String::new(scope, &text) else {
        return NAPI_GENERIC_FAILURE;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(created.into())) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_object(env: Env, result: *mut NapiValue) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(v8::Object::new(scope).into())) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_array(env: Env, result: *mut NapiValue) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(v8::Array::new(scope, 0).into())) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_array_with_length(
    env: Env,
    length: usize,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe {
        out(
            result,
            from_local(v8::Array::new(scope, length as i32).into()),
        )
    }
}

// =========================================================== value reading

/// napi_valuetype numeric values per js_native_api_types.h.
/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_typeof(env: Env, value: NapiValue, result: *mut i32) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let kind = if local.is_undefined() {
        0
    } else if local.is_null() {
        1
    } else if local.is_boolean() {
        2
    } else if local.is_number() {
        3
    } else if local.is_string() {
        4
    } else if local.is_symbol() {
        5
    } else if local.is_function() {
        7
    } else if local.is_external() {
        8
    } else if local.is_big_int() {
        9
    } else {
        6 // object
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, kind) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_value_bool(
    env: Env,
    value: NapiValue,
    result: *mut bool,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    if !local.is_boolean() {
        return NAPI_BOOLEAN_EXPECTED;
    }
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, local.is_true()) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_value_int32(
    env: Env,
    value: NapiValue,
    result: *mut i32,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    if !local.is_number() {
        return NAPI_NUMBER_EXPECTED;
    }
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, local.int32_value(scope).unwrap_or(0)) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_value_uint32(
    env: Env,
    value: NapiValue,
    result: *mut u32,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    if !local.is_number() {
        return NAPI_NUMBER_EXPECTED;
    }
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, local.uint32_value(scope).unwrap_or(0)) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_value_int64(
    env: Env,
    value: NapiValue,
    result: *mut i64,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    if !local.is_number() {
        return NAPI_NUMBER_EXPECTED;
    }
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, local.number_value(scope).unwrap_or(0.0) as i64) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_value_double(
    env: Env,
    value: NapiValue,
    result: *mut f64,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    if !local.is_number() {
        return NAPI_NUMBER_EXPECTED;
    }
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, local.number_value(scope).unwrap_or(0.0)) }
}

/// How many bytes of `text` fit in `cap` bytes without splitting a UTF-8
/// sequence: `cap` backed up to the previous character boundary. A short
/// buffer then holds only whole characters, so what the addon reads back is
/// still valid UTF-8.
fn utf8_prefix_len(text: &str, cap: usize) -> usize {
    let mut len = text.len().min(cap);
    while !text.is_char_boundary(len) {
        len -= 1;
    }
    len
}

/// Two-phase: null buf -> *result = utf8 byte length; else copy + NUL. A
/// buffer too small for the whole string receives whole characters only.
/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_value_string_utf8(
    env: Env,
    value: NapiValue,
    buf: *mut c_char,
    bufsize: usize,
    result: *mut usize,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(string) = v8::Local::<v8::String>::try_from(local) else {
        return NAPI_STRING_EXPECTED;
    };
    let text = string.to_rust_string_lossy(scope);
    if buf.is_null() {
        // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
        return unsafe { out(result, text.len()) };
    }
    if bufsize == 0 {
        // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
        return unsafe { out(result, 0) };
    }
    let copy_len = utf8_prefix_len(&text, bufsize - 1);
    // SAFETY: `buf` is the caller's out buffer with `bufsize` >= copy_len+1 (enforced above); copies `copy_len` UTF-8 bytes, writes the NUL terminator, and reports the length via the null-checked `result`.
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), buf.cast::<u8>(), copy_len);
        *buf.add(copy_len) = 0;
        if !result.is_null() {
            *result = copy_len;
        }
    }
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_is_array(
    env: Env,
    value: NapiValue,
    result: *mut bool,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, local.is_array()) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_array_length(
    env: Env,
    value: NapiValue,
    result: *mut u32,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(array) = v8::Local::<v8::Array>::try_from(local) else {
        return NAPI_ARRAY_EXPECTED;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, array.length()) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_strict_equals(
    env: Env,
    lhs: NapiValue,
    rhs: NapiValue,
    result: *mut bool,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `lhs` and `rhs` are napi_value handles from this env; to_local reinterprets each as a repr-compatible `v8::Local` (size-asserted) and returns None for a null handle.
    let (Some(a), Some(b)) = (unsafe { (to_local(lhs), to_local(rhs)) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, a.strict_equals(b)) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_coerce_to_string(
    env: Env,
    value: NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `env` is the FFI caller's env pointer (catching null-guards it and reports a missing native scope); no other scope reference is taken in this function; `result` is the caller's out-parameter pointer, null-checked before the write.
    unsafe {
        catching_value(env, result, |scope| {
            local.to_string(scope).map(|s| from_local(s.into()))
        })
    }
}

// ============================================================== properties

/// # Safety
///
/// The caller must pass a `napi_value` from this env; it is read as a
/// `v8::Local` and downcast to Object.
unsafe fn as_object<'s>(value: NapiValue) -> Option<v8::Local<'s, v8::Object>> {
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let local = unsafe { to_local(value) }?;
    v8::Local::<v8::Object>::try_from(local).ok()
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_set_named_property(
    env: Env,
    object: NapiValue,
    name: *const c_char,
    value: NapiValue,
) -> NapiStatus {
    // SAFETY: `object` is a napi_value handle from this env; as_object reads it as a `v8::Local` and downcasts to Object (None when null or not an object).
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let (Some(value), false) = (unsafe { to_local(value) }, name.is_null()) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `name` is the caller's NUL-terminated C string, null-checked before this read.
    let name = unsafe { std::ffi::CStr::from_ptr(name) }.to_string_lossy();
    // The key is built under the same TryCatch, with the scope `catching`
    // hands the closure: a string V8 cannot allocate is its empty result,
    // which is `napi_generic_failure`.
    // SAFETY: `env` is the FFI caller's env pointer (catching null-guards it and reports a missing native scope); no other scope reference is taken in this function.
    finish_set(unsafe {
        catching(env, |scope| {
            let key = v8::String::new(scope, &name)?;
            target.set(scope, key.into(), value)
        })
    })
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_named_property(
    env: Env,
    object: NapiValue,
    name: *const c_char,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `object` is a napi_value handle from this env; as_object reads it as a `v8::Local` and downcasts to Object (None when null or not an object).
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    if name.is_null() {
        return NAPI_INVALID_ARG;
    }
    // SAFETY: `name` is the caller's NUL-terminated C string, null-checked before this read.
    let name = unsafe { std::ffi::CStr::from_ptr(name) }.to_string_lossy();
    // The key is built under the same TryCatch (see napi_set_named_property).
    // SAFETY: `env` is the FFI caller's env pointer (catching null-guards it and reports a missing native scope); no other scope reference is taken in this function.
    // `result` is the caller's out-parameter pointer, null-checked before the write.
    unsafe {
        catching_value(env, result, |scope| {
            let key = v8::String::new(scope, &name)?;
            target.get(scope, key.into()).map(from_local)
        })
    }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_has_named_property(
    env: Env,
    object: NapiValue,
    name: *const c_char,
    result: *mut bool,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `object` is a napi_value handle from this env; as_object reads it as a `v8::Local` and downcasts to Object (None when null or not an object).
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    if name.is_null() {
        return NAPI_INVALID_ARG;
    }
    // SAFETY: `name` is the caller's NUL-terminated C string, null-checked before this read.
    let name = unsafe { std::ffi::CStr::from_ptr(name) }.to_string_lossy();
    let Some(key) = v8::String::new(scope, &name) else {
        return NAPI_GENERIC_FAILURE;
    };
    let has = target.has(scope, key.into()).unwrap_or(false);
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, has) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_set_property(
    env: Env,
    object: NapiValue,
    key: NapiValue,
    value: NapiValue,
) -> NapiStatus {
    // SAFETY: `object` is a napi_value handle from this env; as_object reads it as a `v8::Local` and downcasts to Object (None when null or not an object).
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    // SAFETY: `key` and `value` are napi_value handles from this env; to_local reinterprets each as a repr-compatible `v8::Local` (size-asserted) and returns None for a null handle.
    let (Some(key), Some(value)) = (unsafe { (to_local(key), to_local(value)) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `env` is the FFI caller's env pointer (catching null-guards it and reports a missing native scope); no other scope reference is taken in this function.
    finish_set(unsafe { catching(env, |scope| target.set(scope, key, value)) })
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_property(
    env: Env,
    object: NapiValue,
    key: NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `object` is a napi_value handle from this env; as_object reads it as a `v8::Local` and downcasts to Object (None when null or not an object).
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    // SAFETY: `key` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(key) = (unsafe { to_local(key) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `env` is the FFI caller's env pointer (catching null-guards it and reports a missing native scope); no other scope reference is taken in this function.
    // `result` is the caller's out-parameter pointer, null-checked before the write.
    unsafe { catching_value(env, result, |scope| target.get(scope, key).map(from_local)) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_set_element(
    env: Env,
    object: NapiValue,
    index: u32,
    value: NapiValue,
) -> NapiStatus {
    // SAFETY: `object` is a napi_value handle from this env; as_object reads it as a `v8::Local` and downcasts to Object (None when null or not an object).
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(value) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `env` is the FFI caller's env pointer (catching null-guards it and reports a missing native scope); no other scope reference is taken in this function.
    finish_set(unsafe { catching(env, |scope| target.set_index(scope, index, value)) })
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_element(
    env: Env,
    object: NapiValue,
    index: u32,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `object` is a napi_value handle from this env; as_object reads it as a `v8::Local` and downcasts to Object (None when null or not an object).
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    // SAFETY: `env` is the FFI caller's env pointer (catching null-guards it and reports a missing native scope); no other scope reference is taken in this function.
    // `result` is the caller's out-parameter pointer, null-checked before the write.
    unsafe {
        catching_value(env, result, |scope| {
            target.get_index(scope, index).map(from_local)
        })
    }
}

// =============================================================== functions

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_function(
    env: Env,
    name: *const c_char,
    length: usize,
    cb: Option<unsafe extern "C" fn(Env, *mut CbInfo) -> NapiValue>,
    data: *mut c_void,
    result: *mut NapiValue,
) -> NapiStatus {
    // The exclusive scope, and the only one this function takes: tracking
    // the function's External weakly needs `&mut Isolate`.
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv` (null-guarded inside); a native entry is on the stack per this function's contract, and no shared scope reference is taken here.
    let Some(scope) = (unsafe { env_scope_mut(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // As in Node, the callback is required.
    let Some(cb) = cb else {
        return NAPI_INVALID_ARG;
    };
    // Node checks the length only of a name it reads: a null name leaves the
    // function unnamed whatever the length.
    if !name.is_null() && !is_napi_string_length(length) {
        return NAPI_INVALID_ARG;
    }
    // SAFETY: `env` is non-null and live (env_scope_mut found its scope) and no reference into it is held here; `scope` is its installed scope; `name` is null or the caller's string of `length` bytes (NUL-terminated when the length is NAPI_AUTO_LENGTH); `cb` and `data` are the addon's own.
    let Some(function) = (unsafe { create_function(env, scope, name, length, cb, data) }) else {
        return NAPI_GENERIC_FAILURE;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(function.into())) }
}

/// Build the JS function behind `napi_create_function`, `napi_define_class`
/// and the method / getter / setter of a property descriptor: file a
/// [`FnData`] record in the env, create the `External` that carries its
/// address, track that `External` weakly so the record is released after it
/// is collected, and build the trampoline function around it. `None` when V8
/// could not build the function; nothing is left behind in the env then.
///
/// Takes the scope from its caller instead of re-deriving one, so an entry
/// point that defines several functions still holds exactly one scope
/// reference (`env_scope_mut`'s contract).
///
/// # Safety
///
/// `env` must be a live `*mut NapiEnv` with no reference into it held by the
/// caller, and `scope` the scope installed on it. `name` must be null, or
/// valid for reads of `length` bytes -- a NUL-terminated C string when
/// `length` is NAPI_AUTO_LENGTH (`usize::MAX`); an explicit length is read
/// exactly, as N-API specifies. `cb` must be the addon's `napi_callback`,
/// safe to call with `data` for as long as the returned function can be
/// called.
unsafe fn create_function<'s>(
    env: Env,
    scope: &mut v8::PinScope<'s, '_>,
    name: *const c_char,
    length: usize,
    cb: unsafe extern "C" fn(Env, *mut CbInfo) -> NapiValue,
    data: *mut c_void,
) -> Option<v8::Local<'s, v8::Function>> {
    // The one borrow of the env this function takes. It stays live across
    // the V8 calls below (`External::new`, `Weak::with_finalizer` inside
    // `track_weak_as`, and `Function::builder().build()`), so nothing may
    // re-enter this env under it. Two things make that hold:
    // * The collector never touches a `NapiEnv` (see `NapiEnv::collected`),
    //   so the allocations are harmless.
    // * The calls run no JavaScript. That rests on two things, and only
    //   the second is this runtime's to keep:
    //   1. In v8 150.0.0 (src/api/api.cc, api-inl.h) `External::new` and
    //      the weak-handle calls enter no call-depth scope, and `build` --
    //      v8::Function::New, which instantiates through
    //      FunctionTemplate::GetFunction -- enters a
    //      PrepareForExecutionScope, i.e. `CallDepthScope<false>`: the kind
    //      that does NOT fire the call-completed hook. That is a V8
    //      internal with no API guarantee behind it.
    //   2. Should a call under this borrow ever arm that hook
    //      (`CallDepthScope<true>`, as Function::Call, Object::Set and
    //      Value::NumberValue's slow path do today), V8 runs
    //      Isolate::FireCallCompletedCallback when it leaves API call depth
    //      zero: a microtask checkpoint if the policy is
    //      `MicrotasksPolicy::Auto`, then any call-completed callbacks. A
    //      promise reaction run there could call back into the addon and
    //      so into this env. That cannot happen because `JsRuntime::new`
    //      sets `MicrotasksPolicy::Explicit` (lib.rs,
    //      `set_microtasks_policy`) and the runtime installs no
    //      call-completed callback and no custom microtask queue.
    //      `load_addon` debug-asserts the policy.
    //   Changing the policy or installing either of those leaves this
    //   borrow -- and those in `napi_wrap`, `napi_add_finalizer` and the
    //   `ref_entry_mut` callers, which lean on the same "runs no JS" --
    //   resting on point 1 alone.
    // SAFETY: `env` is live and unaliased per this function's contract, and nothing below re-enters this ABI (see the account above).
    let env_ref = unsafe { &mut *env };
    // File the record FIRST, then derive the pointer from it in place: a
    // pointer taken from the Box before it moved into the table would not
    // survive the move (`load_addon` has the same idiom and the reason).
    // The id is minted up front because the External is created around the
    // record's address and only then can be tracked.
    let id = env_ref.mint_tracked_id();
    let record: *mut FnData = &raw mut **env_ref
        .fn_data
        .entry(id)
        .or_insert_with(|| Box::new(FnData { cb, data, env }));
    #[cfg(test)]
    NAPI_FN_DATA_FILED_COUNT.with(|c| c.set(c.get() + 1));
    let external = v8::External::new(scope, record.cast::<c_void>());
    env_ref.track_weak_as(
        id,
        scope,
        external.into(),
        std::ptr::null_mut(),
        None,
        std::ptr::null_mut(),
    );
    let Some(function) = v8::Function::builder(napi_trampoline)
        .data(external.into())
        .build(scope)
    else {
        // No Function was built, so nothing can ever call through the
        // External: stop tracking it (dropping the weak handle unregisters
        // its finalizer, so the id is never reported) and release the
        // record now rather than leave it in the env for the runtime's
        // life.
        env_ref.tracked.remove(&id);
        env_ref.fn_data.remove(&id);
        return None;
    };
    // SAFETY: `name` is null or valid for `length` bytes (NUL-terminated when the length is NAPI_AUTO_LENGTH) per this function's contract; sized_text null-checks it.
    if let Some(name) = unsafe { sized_text(name, length) }
        && let Some(label) = v8::String::new(scope, &name)
    {
        function.set_name(label);
    }
    Some(function)
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_cb_info(
    env: Env,
    cbinfo: *mut CbInfo,
    argc: *mut usize,
    argv: *mut NapiValue,
    this_arg: *mut NapiValue,
    data: *mut *mut c_void,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `cbinfo` is the CbInfo pointer the trampoline placed on its stack and passed to this callback; as_ref yields None if the addon passed null.
    let Some(info) = (unsafe { cbinfo.as_ref() }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: writes back through the caller's out-parameters -- each of `argc`, `argv`, `this_arg`, `data` is null-checked before use, and `argv` is filled for at most the `*argc` slots the caller declared.
    unsafe {
        if !argc.is_null() {
            let wanted = *argc;
            if !argv.is_null() {
                for i in 0..wanted {
                    let value = info
                        .args
                        .get(i)
                        .copied()
                        .unwrap_or_else(|| from_local(v8::undefined(scope).into()));
                    argv.add(i).write(value);
                }
            }
            *argc = info.args.len();
        }
        if !this_arg.is_null() {
            *this_arg = info.this;
        }
        if !data.is_null() {
            *data = info.data;
        }
    }
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_call_function(
    env: Env,
    recv: NapiValue,
    func: NapiValue,
    argc: usize,
    argv: *const NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `recv` and `func` are napi_value handles from this env; to_local reinterprets each as a repr-compatible `v8::Local` (size-asserted) and returns None for a null handle.
    let (Some(recv), Some(func_value)) = (unsafe { (to_local(recv), to_local(func)) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(function) = v8::Local::<v8::Function>::try_from(func_value) else {
        return NAPI_FUNCTION_EXPECTED;
    };
    // A null `argv` with a non-zero count, or a null element, is refused:
    // skipping it would call the function with the later arguments shifted.
    if argc > 0 && argv.is_null() {
        return NAPI_INVALID_ARG;
    }
    let Some(args) = (0..argc)
        // SAFETY: `argv` is non-null whenever `argc` is non-zero (checked above) and points to at least `argc` caller-owned napi_value handles; to_local reinterprets each as a repr-compatible `v8::Local` and returns None for a null handle.
        .map(|i| unsafe { to_local(*argv.add(i)) })
        .collect::<Option<Vec<_>>>()
    else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `env` is the FFI caller's env pointer (catching null-guards it and reports a missing native scope); no other scope reference is taken in this function.
    let outcome = unsafe {
        catching(env, |scope| {
            function.call(scope, recv, &args).map(from_local)
        })
    };
    match outcome {
        // `result` is optional here: a caller that ignores the return value passes null.
        Ok(_) if result.is_null() => NAPI_OK,
        // SAFETY: `result` is the caller's out-parameter pointer, non-null on this arm; out() null-checks it again before writing.
        Ok(value) => unsafe { out(result, value) },
        Err(status) => status,
    }
}

// ================================================================== errors

/// # Safety
///
/// `code` and `msg` are optional caller C strings, each read only after a
/// null check; `scope` is a live PinScope.
unsafe fn build_error(
    scope: &v8::PinScope<'_, '_>,
    code: *const c_char,
    msg: *const c_char,
    kind: fn(&v8::PinScope<'_, '_>, v8::Local<v8::String>) -> v8::Local<'static, v8::Value>,
) -> Option<v8::Global<v8::Value>> {
    let message = if msg.is_null() {
        "unknown native error".into()
    } else {
        // SAFETY: `msg` is the caller's NUL-terminated C string, null-checked before this read.
        unsafe { std::ffi::CStr::from_ptr(msg) }.to_string_lossy()
    };
    let message = v8::String::new(scope, &message)?;
    let error = kind(scope, message);
    if !code.is_null()
        && let Ok(object) = v8::Local::<v8::Object>::try_from(error)
    {
        // SAFETY: `code` is the caller's NUL-terminated C string, null-checked before this read.
        let code_text = unsafe { std::ffi::CStr::from_ptr(code) }.to_string_lossy();
        let key = v8::String::new(scope, "code")?;
        if let Some(value) = v8::String::new(scope, &code_text) {
            object.set(scope, key.into(), value.into());
        }
    }
    Some(v8::Global::new(scope, error))
}

fn plain_error<'s>(
    scope: &v8::PinScope<'s, '_>,
    message: v8::Local<v8::String>,
) -> v8::Local<'static, v8::Value> {
    let error = v8::Exception::error(scope, message);
    // SAFETY: relifetimes the freshly built error `v8::Local` to 'static; build_error immediately re-roots it into a `v8::Global` before the scope ends, so the handle stays valid.
    unsafe { std::mem::transmute::<v8::Local<v8::Value>, v8::Local<'static, v8::Value>>(error) }
}

fn type_error<'s>(
    scope: &v8::PinScope<'s, '_>,
    message: v8::Local<v8::String>,
) -> v8::Local<'static, v8::Value> {
    let error = v8::Exception::type_error(scope, message);
    // SAFETY: relifetimes the freshly built error `v8::Local` to 'static; build_error immediately re-roots it into a `v8::Global` before the scope ends, so the handle stays valid.
    unsafe { std::mem::transmute::<v8::Local<v8::Value>, v8::Local<'static, v8::Value>>(error) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_throw(env: Env, error: NapiValue) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `error` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(error) }) else {
        return NAPI_INVALID_ARG;
    };
    let global = v8::Global::new(scope, local);
    // SAFETY: `env` is a valid `*mut NapiEnv` (established above); reads/writes its `pending` deferred-exception slot.
    unsafe { (*env).pending = Some(global) };
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_throw_error(
    env: Env,
    code: *const c_char,
    msg: *const c_char,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `code` and `msg` are the caller's optional C strings; build_error reads each only after a null check.
    match unsafe { build_error(scope, code, msg, plain_error) } {
        Some(error) => {
            // SAFETY: `env` is a valid `*mut NapiEnv` (established above); reads/writes its `pending` deferred-exception slot.
            unsafe { (*env).pending = Some(error) };
            NAPI_OK
        }
        None => NAPI_GENERIC_FAILURE,
    }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_throw_type_error(
    env: Env,
    code: *const c_char,
    msg: *const c_char,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `code` and `msg` are the caller's optional C strings; build_error reads each only after a null check.
    match unsafe { build_error(scope, code, msg, type_error) } {
        Some(error) => {
            // SAFETY: `env` is a valid `*mut NapiEnv` (established above); reads/writes its `pending` deferred-exception slot.
            unsafe { (*env).pending = Some(error) };
            NAPI_OK
        }
        None => NAPI_GENERIC_FAILURE,
    }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_is_exception_pending(env: Env, result: *mut bool) -> NapiStatus {
    // SAFETY: `env` is the caller's `*mut NapiEnv`; as_ref/as_mut yields None if the caller passed null.
    let Some(env_ref) = (unsafe { env.as_ref() }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, env_ref.pending.is_some()) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_and_clear_last_exception(
    env: Env,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `env` is a valid `*mut NapiEnv` (established above); reads/writes its `pending` deferred-exception slot.
    let pending = unsafe { (*env).pending.take() };
    let value = match pending {
        Some(global) => from_local(v8::Local::new(scope, &global)),
        None => from_local(v8::undefined(scope).into()),
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, value) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_version(env: Env, result: *mut u32) -> NapiStatus {
    if env.is_null() {
        return NAPI_INVALID_ARG;
    }
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, 8) } // N-API version 8 (the stable baseline)
}

// ================================================================= externals

/// `v8::External` wrapping a raw pointer.  The optional finalizer runs
/// after the External has been collected, the way a wrap's does (see
/// [`NapiWrapEntry`]).
/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_external(
    env: Env,
    data: *mut c_void,
    finalize_cb: Option<NapiFinalize>,
    finalize_hint: *mut c_void,
    result: *mut NapiValue,
) -> NapiStatus {
    // The exclusive scope, and the only one this function takes: tracking
    // the External weakly needs `&mut Isolate`.
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv` (null-guarded inside); a native entry is on the stack per this function's contract, and no shared scope reference is taken here.
    let Some(scope) = (unsafe { env_scope_mut(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let ext = v8::External::new(scope, data);
    if finalize_cb.is_some() {
        // SAFETY: `env` is the live env `scope` was taken from (non-null, or there would be no scope); no other reference into it is live, and the borrow ends with this statement, which runs no JS.
        unsafe { &mut *env }.track_weak(scope, ext.into(), data, finalize_cb, finalize_hint);
    }
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(ext.into())) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_value_external(
    env: Env,
    value: NapiValue,
    result: *mut *mut c_void,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(ext) = v8::Local::<v8::External>::try_from(local) else {
        return NAPI_INVALID_ARG;
    };
    if result.is_null() {
        return NAPI_INVALID_ARG;
    }
    // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
    unsafe { *result = ext.value() };
    NAPI_OK
}

// ================================================================ references
//
// NapiRef handle: an opaque *mut c_void that is NOT a pointer. It packs a slot
// index, that slot's generation, and a per-env tag; see `encode_ref_handle`.

type NapiRefHandle = *mut c_void;

// ------------------------------------------------------- handle encoding
//
// A `napi_ref` is pointer-sized because the ABI says so, but nothing in the
// ABI says it must BE a pointer, and making it one is what created the hole
// this encoding closes. The layout, most significant field first:
//
//     [ env tag : REF_TAG_BITS | generation : REF_GEN_BITS | index : REF_INDEX_BITS ]
//
// Widths are derived from the pointer width, so a 32-bit target stays correct
// with every field simply narrower (8 / 12 / 12 instead of 16 / 24 / 24).

/// Width of the per-env tag. Published so the e2e suite can forge handles
/// against the real layout instead of duplicating these numbers.
pub const REF_TAG_BITS: u32 = usize::BITS / 4;
/// Width of the slot index.
pub const REF_INDEX_BITS: u32 = (usize::BITS - REF_TAG_BITS) / 2;
/// Width of the per-slot generation counter.
pub const REF_GEN_BITS: u32 = usize::BITS - REF_TAG_BITS - REF_INDEX_BITS;

const REF_INDEX_MASK: usize = (1usize << REF_INDEX_BITS) - 1;
const REF_GEN_MASK: usize = (1usize << REF_GEN_BITS) - 1;
const REF_TAG_MASK: usize = (1usize << REF_TAG_BITS) - 1;

/// Highest generation a slot may reach. On hitting it the slot is RETIRED
/// rather than reused, which is what stops the counter wrapping around onto a
/// generation some addon still holds a handle for. Costs one empty slot after
/// 16.7M create/delete cycles of that single slot on a 64-bit target.
#[cfg(not(test))]
const REF_MAX_GENERATION: usize = REF_GEN_MASK;
/// Test-only limit. 16.7M create/delete cycles on one slot is not a runnable
/// test, so under `cfg(test)` the LIMIT shrinks and the LAYOUT does not:
/// `REF_GEN_BITS` / `REF_GEN_MASK` and every encode/decode mask keep their
/// shipped values, and 3 is an ordinary generation in that layout, so a handle
/// minted against this limit round-trips through the real encoding unchanged.
#[cfg(test)]
const REF_MAX_GENERATION: usize = 3;

/// Largest reference table one env can have (16.7M slots on a 64-bit target).
/// Past this `napi_create_reference` refuses rather than minting a handle
/// whose index would be truncated.
#[cfg(not(test))]
const REF_MAX_SLOTS: usize = REF_INDEX_MASK + 1;
/// Test-only limit, for the same reason as [`REF_MAX_GENERATION`]: filling
/// 16.7M slots is not a runnable test. Only the refusal threshold moves --
/// `REF_INDEX_BITS` / `REF_INDEX_MASK` are untouched, so indices 0..4 encode
/// and decode exactly as they do in a shipped build.
#[cfg(test)]
const REF_MAX_SLOTS: usize = 4;

/// Source of per-env tags.
///
/// The old address-identity scan got cross-env rejection for free, because the
/// allocator never hands two live objects the same address. An index alone has
/// no such property -- every env's table starts at index 0 -- so the tag is
/// what replaces it. It is `REF_TAG_BITS` wide, so it repeats after 65,536
/// envs on a 64-bit target; two LIVE envs would have to be that far apart in
/// load order to share one, and an env is created per loaded addon. Stated
/// rather than hidden: this is a narrower guarantee than address uniqueness,
/// and it is the one place this design is weaker than what it replaces.
static NAPI_ENV_TAG_SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);

fn next_env_ref_tag() -> usize {
    NAPI_ENV_TAG_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed) & REF_TAG_MASK
}

/// Pack a slot index and generation, with this env's tag, into the
/// pointer-sized value the ABI hands back as a `napi_ref`.
///
/// Never returns null. `generation` is >= 1 for every live slot and sits in a
/// middle field, so the whole word is non-zero -- which matters because the
/// ABI uses a null `napi_ref` as "no reference" (`napi_wrap` writes one).
fn encode_ref_handle(tag: usize, index: usize, generation: usize) -> NapiRefHandle {
    debug_assert!(index <= REF_INDEX_MASK, "slot index overflows its field");
    debug_assert!(
        (1..=REF_GEN_MASK).contains(&generation),
        "generation overflows its field, or is the null-sentinel 0"
    );
    let bits = ((tag & REF_TAG_MASK) << (REF_GEN_BITS + REF_INDEX_BITS))
        | ((generation & REF_GEN_MASK) << REF_INDEX_BITS)
        | (index & REF_INDEX_MASK);
    bits as NapiRefHandle
}

/// Inverse of [`encode_ref_handle`], rejecting anything `tag` did not mint.
///
/// Pure integer work: no dereference, so a null, stale, foreign or wholly
/// forged handle costs a few instructions and can never fault. That is the
/// substantive difference from the address scan this replaced, which had to
/// prove a handle was live before it dared touch it.
fn decode_ref_handle(tag: usize, ref_: NapiRefHandle) -> Option<(usize, usize)> {
    let bits = ref_ as usize;
    // The ABI's "no reference" sentinel, and never a value we mint.
    if bits == 0 {
        return None;
    }
    if (bits >> (REF_GEN_BITS + REF_INDEX_BITS)) != (tag & REF_TAG_MASK) {
        return None;
    }
    let generation = (bits >> REF_INDEX_BITS) & REF_GEN_MASK;
    if generation == 0 {
        return None;
    }
    Some((bits & REF_INDEX_MASK, generation))
}

impl NapiEnv {
    /// Install `entry` in a free slot (or a fresh one) and mint its handle.
    ///
    /// Returns the entry back in `Err` when the table is full, so the caller
    /// decides what the refusal means and where the entry -- and the
    /// persistent handle it owns -- is dropped.
    fn alloc_ref(&mut self, entry: NapiRefEntry) -> Result<NapiRefHandle, NapiRefEntry> {
        let index = match self.ref_free.pop() {
            Some(index) => index,
            None => {
                if self.refs.len() >= REF_MAX_SLOTS {
                    return Err(entry);
                }
                self.refs.push(NapiRefSlot {
                    entry: None,
                    generation: 1,
                });
                self.refs.len() - 1
            }
        };
        let tag = self.ref_tag;
        let slot = &mut self.refs[index];
        slot.entry = Some(entry);
        Ok(encode_ref_handle(tag, index, slot.generation))
    }

    /// Mint a WEAK reference (count 0) to `value` in this env's table: what
    /// `napi_wrap` and `napi_add_finalizer` hand back through their optional
    /// `result`. `None` when the table is full.
    fn mint_weak_ref(
        &mut self,
        isolate: &mut v8::Isolate,
        value: v8::Local<v8::Value>,
    ) -> Option<NapiRefHandle> {
        let entry = NapiRefEntry {
            value: NapiRefValue::Weak(v8::Weak::new(isolate, value)),
            refcount: 0,
        };
        self.alloc_ref(entry).ok()
    }

    /// Resolve `ref_` to its live entry, or `None` when the handle is null,
    /// foreign to this env, out of range, names a generation the slot has
    /// already moved past, or names a slot that is currently free.
    fn ref_slot_entry(&self, ref_: NapiRefHandle) -> Option<&NapiRefEntry> {
        let (index, generation) = decode_ref_handle(self.ref_tag, ref_)?;
        let slot = self.refs.get(index)?;
        if slot.generation != generation {
            return None;
        }
        slot.entry.as_ref()
    }

    /// Exclusive counterpart to [`NapiEnv::ref_slot_entry`], for the refcount
    /// mutators. Same acceptance rule.
    fn ref_slot_entry_mut(&mut self, ref_: NapiRefHandle) -> Option<&mut NapiRefEntry> {
        let (index, generation) = decode_ref_handle(self.ref_tag, ref_)?;
        let slot = self.refs.get_mut(index)?;
        if slot.generation != generation {
            return None;
        }
        slot.entry.as_mut()
    }

    /// Take every entry out of the table, each slot retired exactly as
    /// [`NapiEnv::free_ref`] retires one: its generation moves on (or the
    /// slot is retired for good at the last one) and it goes on the free
    /// list. The slots stay, so no handle minted before this can resolve
    /// again -- in particular not to an entry minted after it.
    fn retire_all_refs(&mut self) -> Vec<NapiRefEntry> {
        let mut taken = Vec::new();
        for (index, slot) in self.refs.iter_mut().enumerate() {
            if let Some(entry) = slot.entry.take() {
                if slot.generation < REF_MAX_GENERATION {
                    slot.generation += 1;
                    self.ref_free.push(index);
                }
                taken.push(entry);
            }
        }
        taken
    }

    /// Take the entry `ref_` names out of the table and make its slot reusable.
    ///
    /// Hands the entry back rather than dropping it here, as
    /// [`NapiEnv::alloc_ref`] does: the caller reads what it needs from it
    /// first.
    fn free_ref(&mut self, ref_: NapiRefHandle) -> Option<NapiRefEntry> {
        let (index, generation) = decode_ref_handle(self.ref_tag, ref_)?;
        let slot = self.refs.get_mut(index)?;
        if slot.generation != generation {
            return None;
        }
        // A free slot has already had its generation bumped, so a stale handle
        // normally fails the compare above. This still catches the one case it
        // cannot: a RETIRED slot, whose generation is pinned at the maximum.
        let entry = slot.entry.take()?;
        if slot.generation < REF_MAX_GENERATION {
            slot.generation += 1;
            self.ref_free.push(index);
        }
        // else: the slot has exhausted its generations. Leaving it off the free
        // list retires it for good, which is what stops the counter wrapping
        // back onto a handle some addon may still be holding.
        Some(entry)
    }
}

/// The holder a reference with `refcount` uses for `value`: weak for an
/// object at a count of zero, strong otherwise.
fn ref_value_for(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<v8::Value>,
    refcount: u32,
) -> NapiRefValue {
    if refcount == 0 && value.is_object() {
        let isolate: &mut v8::Isolate = scope;
        NapiRefValue::Weak(v8::Weak::new(isolate, value))
    } else {
        NapiRefValue::Strong(v8::Global::new(scope, value))
    }
}

/// The value a reference currently holds, as a handle in `scope`. `None`
/// once a weakly-held object has been collected.
fn ref_value_local<'s>(
    scope: &v8::PinScope<'s, '_>,
    value: &NapiRefValue,
) -> Option<v8::Local<'s, v8::Value>> {
    match value {
        NapiRefValue::Strong(global) => Some(v8::Local::new(scope, global)),
        NapiRefValue::Weak(weak) => weak.to_local(scope),
    }
}

/// Re-hold the value of `entry` the way its CURRENT refcount calls for:
/// strong at a positive count, weak at zero. Called after a count crosses
/// between the two. Does nothing when the holder already matches, or when a
/// weakly-held object is already gone (there is nothing left to hold).
fn retarget_ref(scope: &mut v8::PinScope<'_, '_>, entry: &mut NapiRefEntry) {
    let want_weak = entry.refcount == 0;
    let is_weak = matches!(entry.value, NapiRefValue::Weak(_));
    if want_weak == is_weak {
        return;
    }
    let Some(local) = ref_value_local(scope, &entry.value) else {
        return;
    };
    if want_weak && !local.is_object() {
        // Only an object can be held weakly; a primitive stays strong.
        return;
    }
    entry.value = ref_value_for(scope, local, entry.refcount);
}

/// Resolve a caller-supplied `napi_ref` to its entry, for the read paths.
///
/// The handle is DECODED, never dereferenced: it is an index plus a generation
/// plus this env's tag, so resolving it is a bounds check and two integer
/// compares against the env's own table. The only pointer this touches is
/// `env` itself, and the reference it returns is derived from that -- so a
/// null, stale, foreign or wholly forged handle is a miss, and never a fault.
///
/// This replaced a `ptr::eq` scan over boxed entries whose ABA hole was
/// documented in place and then measured: 5,000 create/delete/create cycles
/// through the vendored addon accepted 40 stale handles and resolved every one
/// of them to the WRONG live reference. A generation that is bumped on free
/// makes that case a deterministic miss.
///
/// # Safety
///
/// `env` must be the live `napi_env` this engine handed the addon; `ref_` is
/// checked, so it may be null, stale or foreign.
unsafe fn ref_entry<'a>(env: Env, ref_: NapiRefHandle) -> Option<&'a NapiRefEntry> {
    // SAFETY: `env` is the caller's `*mut NapiEnv`; `as_ref` guards null. The
    // reborrow is SHARED, which is what lets the caller keep the result live
    // across `v8::Local::new` without claiming exclusivity over the whole env.
    unsafe { env.as_ref()?.ref_slot_entry(ref_) }
}

/// Exclusive counterpart to [`ref_entry`], for the refcount mutators.
///
/// Callers must not hold the result across a call that can run JS or addon
/// code -- JS can re-enter this ABI, and a second `&mut *env` while this one
/// is live is aliasing UB. The two callers below touch an integer and, when
/// the count crosses zero, re-hold the value ([`retarget_ref`]): creating and
/// disposing persistent handles, which runs no JS. The collector does not
/// touch the env either (see `NapiEnv::collected`).
///
/// "Runs no JS" has two legs. In v8 150.0.0 creating and disposing a
/// persistent handle enters no call-depth scope, so it cannot reach V8's
/// call-completed hook; that is a V8 internal. The leg this runtime owns is
/// that the hook would run nothing even if a call under this borrow armed
/// it: `JsRuntime::new` sets `MicrotasksPolicy::Explicit` (lib.rs) and
/// installs no call-completed callback or custom microtask queue. Under the
/// auto policy a hook-arming V8 call (`CallDepthScope<true>`: Function::Call,
/// Object::Set, ...) runs a microtask checkpoint when it leaves API call
/// depth zero, and a promise reaction run there could re-enter the addon
/// while the result of this function is live. `load_addon` debug-asserts
/// the policy; the full account is in `napi_create_function`.
///
/// # Safety
///
/// As [`ref_entry`].
unsafe fn ref_entry_mut<'a>(env: Env, ref_: NapiRefHandle) -> Option<&'a mut NapiRefEntry> {
    // SAFETY: `env` is the caller's `*mut NapiEnv`; `as_mut` guards null, and
    // the handle is decoded rather than dereferenced.
    unsafe { env.as_mut()?.ref_slot_entry_mut(ref_) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_reference(
    env: Env,
    value: NapiValue,
    initial_refcount: u32,
    result: *mut NapiRefHandle,
) -> NapiStatus {
    // The exclusive scope, and the only one this function takes: a weak
    // reference needs `&mut Isolate`.
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv` (null-guarded inside); a native entry is on the stack per this function's contract, and no shared scope reference is taken here.
    let Some(scope) = (unsafe { env_scope_mut(env) }) else {
        // SAFETY: `env` is the caller's env pointer or null; fail() null-guards.
        return unsafe {
            fail(
                env,
                NAPI_INVALID_ARG,
                c"napi_create_reference: no native scope is active on this env",
            )
        };
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        // SAFETY: as above.
        return unsafe {
            fail(
                env,
                NAPI_INVALID_ARG,
                c"napi_create_reference: `value` is null",
            )
        };
    };
    // Reject a null out-parameter BEFORE creating anything. Pushing first and
    // failing afterwards left the entry in `env.refs` with no handle ever
    // dispensed for it -- unreachable, and freed only when the env drops. Node
    // validates its arguments up front and creates no reference at all.
    if result.is_null() {
        // SAFETY: as above.
        return unsafe {
            fail(
                env,
                NAPI_INVALID_ARG,
                c"napi_create_reference: `result` out-pointer is null",
            )
        };
    }
    // A count of zero on an object is a WEAK reference, as in Node.
    let entry = NapiRefEntry {
        value: ref_value_for(scope, local, initial_refcount),
        refcount: initial_refcount,
    };
    // SAFETY: `env` is the caller's `*mut NapiEnv`, checked non-null above; reborrowed here to reach its owned ref table. The borrow ends with this statement -- `alloc_ref` returns owned values either way.
    let minted = unsafe { &mut *env }.alloc_ref(entry);
    let handle = match minted {
        Ok(handle) => handle,
        Err(rejected) => {
            // Drop the rejected entry, and the persistent handle it owns.
            drop(rejected);
            // SAFETY: `env` is the caller's env pointer; fail() null-guards.
            return unsafe {
                fail(
                    env,
                    NAPI_GENERIC_FAILURE,
                    c"napi_create_reference: this env's reference table is full",
                )
            };
        }
    };
    #[cfg(test)]
    NAPI_REF_PUSH_COUNT.with(|c| c.set(c.get() + 1));
    // SAFETY: `result` is the caller's out-parameter pointer, null-checked above; out() null-checks it again before writing.
    let status = unsafe { out(result, handle) };
    if status == NAPI_OK {
        // SAFETY: `env` is the caller's env pointer; ok() null-guards.
        return unsafe { ok(env) };
    }
    status
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_delete_reference(env: Env, ref_: NapiRefHandle) -> NapiStatus {
    // SAFETY: `env` is the caller's `*mut NapiEnv`; `as_mut` guards null.
    let Some(env_ref) = (unsafe { env.as_mut() }) else {
        return NAPI_INVALID_ARG;
    };
    let removed = env_ref.free_ref(ref_);
    let found = removed.is_some();
    // Drop the entry -- and the `v8::Global` it owns -- only now that the
    // borrow on the env is gone: disposing a persistent handle calls into V8.
    drop(removed);
    if !found {
        // Nothing was removed, so `ref_` is null, already deleted, or was never
        // ours. This used to answer NAPI_OK regardless, which made a
        // double-delete look successful and hid the addon's own bug -- and was
        // inconsistent with the read/ref/unref paths, which do validate. Node
        // treats an invalid ref as undefined behaviour; refusing is strictly
        // safer and is a deliberate divergence (docs/node-divergences.md).
        // SAFETY: `env` is the caller's env pointer; fail() null-guards.
        return unsafe {
            fail(
                env,
                NAPI_INVALID_ARG,
                c"napi_delete_reference: handle is not a live reference in this env (already deleted, or never created here)",
            )
        };
    }
    // SAFETY: `env` is the caller's env pointer; ok() null-guards.
    unsafe { ok(env) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_reference_value(
    env: Env,
    ref_: NapiRefHandle,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        // SAFETY: `env` is the caller's env pointer or null; fail() null-guards.
        return unsafe {
            fail(
                env,
                NAPI_INVALID_ARG,
                c"napi_get_reference_value: no native scope is active on this env",
            )
        };
    };
    // The borrow below is SHARED and stays live across `v8::Local::new`. That
    // is deliberate: `Local::new` only copies a persistent into the current
    // handle scope, so it cannot run JS and cannot re-enter this ABI. A
    // re-entrant `napi_create_reference` would take `&mut *env`, which is why
    // the exclusive lookup (`ref_entry_mut`) is kept off this path.
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv` (env_scope proved it non-null); ref_entry decodes the handle rather than dereferencing it, so null, stale and foreign handles are all misses.
    let Some(entry) = (unsafe { ref_entry(env, ref_) }) else {
        // SAFETY: as above.
        return unsafe {
            fail(
                env,
                NAPI_INVALID_ARG,
                c"napi_get_reference_value: handle is not a live reference in this env (already deleted, or never created here)",
            )
        };
    };
    // A weak reference whose object has been collected reads back as a null
    // `napi_value` with `napi_ok`, as in Node.
    let value = match ref_value_local(scope, &entry.value) {
        Some(local) => from_local(local),
        None => std::ptr::null_mut(),
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    let status = unsafe { out(result, value) };
    if status == NAPI_OK {
        // SAFETY: `env` is the caller's env pointer; ok() null-guards.
        return unsafe { ok(env) };
    }
    // SAFETY: as above.
    unsafe {
        fail(
            env,
            status,
            c"napi_get_reference_value: `result` out-pointer is null",
        )
    }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_reference_ref(
    env: Env,
    ref_: NapiRefHandle,
    result: *mut u32,
) -> NapiStatus {
    // Taken BEFORE the entry is looked up, so the lookup's borrow of the env
    // is the last thing derived from the raw pointer. It is only used when
    // the count crosses zero, to re-hold the value; with no native scope
    // installed that step is skipped -- the count is then still right and
    // only the strength lags, which errs on the side of keeping the object
    // alive.
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv` (null-guarded inside); this is the only scope reference this function takes.
    let scope = unsafe { env_scope_mut(env) };
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; ref_entry_mut guards null on it and decodes the handle rather than dereferencing it, so a deleted or foreign handle is a miss.
    let Some(entry) = (unsafe { ref_entry_mut(env, ref_) }) else {
        // SAFETY: `env` is the caller's env pointer or null; fail() null-guards.
        return unsafe {
            fail(
                env,
                NAPI_INVALID_ARG,
                c"napi_reference_ref: handle is not a live reference in this env (already deleted, or never created here)",
            )
        };
    };
    entry.refcount = entry.refcount.saturating_add(1);
    // Copied out so the exclusive borrow can END before `ok()` below, which
    // re-derives its own `&mut *env`: the two must not be live at once.
    // `entry` is last used by `retarget_ref` just below, alongside the
    // exclusive scope (which points at the scope object, not into the env).
    let count = entry.refcount;
    if count == 1
        && let Some(scope) = scope
    {
        // 0 -> 1: a weak reference becomes strong again (a no-op for one
        // that is already strong, or whose object is gone).
        retarget_ref(scope, entry);
    }
    if !result.is_null() {
        // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *result = count };
    }
    // SAFETY: `env` is the caller's env pointer; ok() null-guards.
    unsafe { ok(env) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_reference_unref(
    env: Env,
    ref_: NapiRefHandle,
    result: *mut u32,
) -> NapiStatus {
    // Taken BEFORE the entry is looked up, so the lookup's borrow of the env
    // is the last thing derived from the raw pointer. It is only used when
    // the count crosses zero, to re-hold the value; with no native scope
    // installed that step is skipped -- the count is then still right and
    // only the strength lags, which errs on the side of keeping the object
    // alive.
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv` (null-guarded inside); this is the only scope reference this function takes.
    let scope = unsafe { env_scope_mut(env) };
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; ref_entry_mut guards null on it and decodes the handle rather than dereferencing it, so a deleted or foreign handle is a miss.
    let Some(entry) = (unsafe { ref_entry_mut(env, ref_) }) else {
        // SAFETY: `env` is the caller's env pointer or null; fail() null-guards.
        return unsafe {
            fail(
                env,
                NAPI_INVALID_ARG,
                c"napi_reference_unref: handle is not a live reference in this env (already deleted, or never created here)",
            )
        };
    };
    entry.refcount = entry.refcount.saturating_sub(1);
    // As in napi_reference_ref: end the exclusive borrow before `ok()`.
    let count = entry.refcount;
    if count == 0
        && let Some(scope) = scope
    {
        // 1 -> 0: the reference stops keeping an object alive.
        retarget_ref(scope, entry);
    }
    if !result.is_null() {
        // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *result = count };
    }
    // SAFETY: `env` is the caller's env pointer; ok() null-guards.
    unsafe { ok(env) }
}

// =========================================================== wrap / classes

/// Property descriptor mirroring `napi_property_descriptor` from js_native_api.h.
/// Must match the C layout exactly (repr(C)).
#[repr(C)]
pub struct NapiPropertyDescriptor {
    pub utf8name: *const c_char,
    pub name: NapiValue,
    pub method: Option<unsafe extern "C" fn(Env, *mut CbInfo) -> NapiValue>,
    pub getter: Option<unsafe extern "C" fn(Env, *mut CbInfo) -> NapiValue>,
    pub setter: Option<unsafe extern "C" fn(Env, *mut CbInfo) -> NapiValue>,
    pub value: NapiValue,
    pub attributes: u32,
    pub data: *mut c_void,
}

const NAPI_ATTR_WRITABLE: u32 = 0x1;
const NAPI_ATTR_ENUMERABLE: u32 = 0x2;
const NAPI_ATTR_CONFIGURABLE: u32 = 0x4;
const NAPI_ATTR_STATIC: u32 = 0x400;

fn napi_attrs_to_v8(attrs: u32) -> v8::PropertyAttribute {
    let mut a = v8::PropertyAttribute::NONE;
    if attrs & NAPI_ATTR_WRITABLE == 0 {
        a = a | v8::PropertyAttribute::READ_ONLY;
    }
    if attrs & NAPI_ATTR_ENUMERABLE == 0 {
        a = a | v8::PropertyAttribute::DONT_ENUM;
    }
    if attrs & NAPI_ATTR_CONFIGURABLE == 0 {
        a = a | v8::PropertyAttribute::DONT_DELETE;
    }
    a
}

/// What `fn.name` a descriptor's `method` gets. Node v22 builds the
/// prototype methods of `napi_define_class` from function templates set on
/// the prototype template, which V8 names after a string key; every other
/// descriptor function (`napi_define_properties`, static class members,
/// symbol keys, getters and setters) is left unnamed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MethodNaming {
    /// The method is unnamed.
    Unnamed,
    /// The method is named after its key when the key is a string.
    StringKey,
}

/// Define on `target` the property one `napi_property_descriptor` describes.
/// The one implementation behind `napi_define_class` and
/// `napi_define_properties`:
///
/// - a `getter` and/or `setter` (checked first, as in Node) defines an
///   accessor property. Each half becomes a trampoline function; a missing
///   half is `undefined`, as in a JS accessor descriptor. `writable` does not
///   apply to an accessor, so only `enumerable` and `configurable` are read
///   from the attributes;
/// - a `method` defines a data property holding a trampoline function;
/// - otherwise `value` is defined directly.
///
/// A descriptor with no usable name, or whose function could not be created,
/// is skipped.
///
/// Every function built here is unnamed (`fn.name === ""`), as in Node,
/// except a method under [`MethodNaming::StringKey`], which takes its
/// string key as its name. Getters and setters are always unnamed.
///
/// # Safety
///
/// `env` must be the live `napi_env` `scope` is installed on, with no
/// reference into it held by the caller, and `scope` the only scope reference
/// the calling entry point holds. Of `prop`:
/// `utf8name` must be null or a NUL-terminated C string; `name` and `value`
/// null or `napi_value`s from this env; `method` / `getter` / `setter` the
/// addon's `napi_callback`s (or null).
unsafe fn define_descriptor<'s>(
    env: Env,
    scope: &mut v8::PinScope<'s, '_>,
    target: v8::Local<v8::Object>,
    prop: &NapiPropertyDescriptor,
    naming: MethodNaming,
) {
    let key: v8::Local<v8::Value> = if !prop.utf8name.is_null() {
        // SAFETY: `prop.utf8name` is the caller's NUL-terminated C string, null-checked before this read.
        let name = unsafe { std::ffi::CStr::from_ptr(prop.utf8name) }.to_string_lossy();
        match v8::String::new(scope, &name) {
            Some(s) => s.into(),
            None => return,
        }
    // SAFETY: `prop.name` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    } else if let Some(name_val) = unsafe { to_local(prop.name) } {
        name_val
    } else {
        return;
    };
    // define_own_property needs Local<Name>. Both String and Symbol are
    // Names, so this only fails for a `name` that is neither.
    let Ok(key) = v8::Local::<v8::Name>::try_from(key) else {
        return;
    };

    // The trampoline function for one of the descriptor's callbacks, built
    // unnamed through the caller's scope: the one scope reference of this
    // entry.
    let function_for =
        |scope: &mut v8::PinScope<'s, '_>, callback| -> Option<v8::Local<'s, v8::Function>> {
            // SAFETY: `env` is the live env `scope` is installed on and no reference into it is held here (this function's contract); the name is null, so no text is read; the callback and `prop.data` are the addon's own.
            unsafe { create_function(env, scope, std::ptr::null(), 0, callback, prop.data) }
        };

    if prop.getter.is_some() || prop.setter.is_some() {
        let undefined: v8::Local<'s, v8::Value> = v8::undefined(scope).into();
        let mut halves = [undefined, undefined];
        for (half, callback) in halves.iter_mut().zip([prop.getter, prop.setter]) {
            if let Some(callback) = callback {
                let Some(function) = function_for(scope, callback) else {
                    return;
                };
                *half = function.into();
            }
        }
        let mut descriptor = v8::PropertyDescriptor::new_from_get_set(halves[0], halves[1]);
        descriptor.set_enumerable(prop.attributes & NAPI_ATTR_ENUMERABLE != 0);
        descriptor.set_configurable(prop.attributes & NAPI_ATTR_CONFIGURABLE != 0);
        target.define_property(scope, key, &descriptor);
    } else if let Some(method) = prop.method {
        if let Some(function) = function_for(scope, method) {
            if naming == MethodNaming::StringKey
                && let Ok(name) = v8::Local::<v8::String>::try_from(key)
            {
                function.set_name(name);
            }
            let attr = napi_attrs_to_v8(prop.attributes);
            target.define_own_property(scope, key, function.into(), attr);
        }
    // SAFETY: `prop.value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    } else if let Some(value) = unsafe { to_local(prop.value) } {
        let attr = napi_attrs_to_v8(prop.attributes);
        target.define_own_property(scope, key, value, attr);
    }
}

/// Create a native-backed JS class (constructor function + prototype methods).
///
/// - Properties with `napi_static` go on the constructor itself.
/// - Otherwise they go on `constructor.prototype`.
/// - `method` descriptors create bound functions via the existing trampoline.
/// - `value` descriptors set named values directly.
/// - `getter`/`setter` descriptors define an accessor property whose halves
///   are ordinary trampoline functions (a `napi_callback` each, so
///   `napi_get_cb_info` reports `this` and, for the setter, the new value).
/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_define_class(
    env: Env,
    utf8name: *const c_char,
    length: usize,
    constructor: Option<unsafe extern "C" fn(Env, *mut CbInfo) -> NapiValue>,
    data: *mut c_void,
    property_count: usize,
    properties: *const NapiPropertyDescriptor,
    result: *mut NapiValue,
) -> NapiStatus {
    // The exclusive scope, and the only one this function takes: it is
    // handed down to `create_function` for the constructor and for every
    // method, getter and setter, each of which tracks its External weakly
    // (`&mut Isolate`).
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv` (null-guarded inside); a native entry is on the stack per this function's contract, and no shared scope reference is taken here.
    let Some(scope) = (unsafe { env_scope_mut(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // As in Node: the class must have a name (unlike napi_create_function,
    // where a null name leaves the function unnamed), it must have a
    // constructor, and a property count needs a descriptor array.
    let Some(constructor) = constructor else {
        return NAPI_INVALID_ARG;
    };
    if utf8name.is_null()
        || !is_napi_string_length(length)
        || (property_count > 0 && properties.is_null())
    {
        return NAPI_INVALID_ARG;
    }
    // Create the constructor function via the shared trampoline.
    // SAFETY: `env` is non-null and live (env_scope_mut found its scope) and no reference into it is held here; `scope` is its installed scope; `utf8name` is non-null (checked above) and the caller's string of `length` bytes (NUL-terminated when the length is NAPI_AUTO_LENGTH); the constructor and `data` are the addon's own.
    let Some(ctor_fn) =
        (unsafe { create_function(env, scope, utf8name, length, constructor, data) })
    else {
        return NAPI_GENERIC_FAILURE;
    };

    // Get `constructor.prototype` for instance methods.
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_val = ctor_fn.get(scope, proto_key.into());
    let proto_obj = proto_val
        .and_then(|v| v8::Local::<v8::Object>::try_from(v).ok())
        .unwrap_or_else(|| v8::Object::new(scope));

    let props = if property_count > 0 {
        // SAFETY: `properties` is non-null (checked above) and points to at least `property_count` caller-owned elements; from_raw_parts reads them for the length the C caller declared.
        unsafe { std::slice::from_raw_parts(properties, property_count) }
    } else {
        &[]
    };

    for prop in props {
        // Node names a prototype method after its string key; a static one
        // goes through napi_define_properties and stays unnamed.
        let (target, naming): (v8::Local<v8::Object>, _) =
            if prop.attributes & NAPI_ATTR_STATIC != 0 {
                (ctor_fn.into(), MethodNaming::Unnamed)
            } else {
                (proto_obj, MethodNaming::StringKey)
            };
        // SAFETY: `env` is the caller's live env (env_scope_mut proved it) with no reference into it held here; `scope` is this entry point's one scope reference; `prop` is one of the caller's descriptors, whose name, value and callbacks are the addon's own per this entry point's contract.
        unsafe { define_descriptor(env, scope, target, prop, naming) };
    }

    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(ctor_fn.into())) }
}

/// Store a raw Rust pointer on a JS object for later retrieval via `napi_unwrap`.
///
/// Any previous wrap on the same object is replaced (Node refuses a second
/// wrap with `napi_invalid_arg`; the replace is kept from the earlier
/// implementation). The wrap does not keep the object alive. Once the object
/// is collected its finalizer runs at this env's next native entry -- not
/// inside the garbage collector -- and any still owed run when the runtime
/// is torn down. `napi_remove_wrap` cancels the finalizer, as in Node.
/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_wrap(
    env: Env,
    js_object: NapiValue,
    native_object: *mut c_void,
    finalize_cb: Option<NapiFinalize>,
    finalize_hint: *mut c_void,
    result: *mut NapiRefHandle,
) -> NapiStatus {
    // The exclusive scope, and the only one this function takes: creating a
    // weak handle needs `&mut Isolate`.
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv` (null-guarded inside); a native entry is on the stack per this function's contract, and no shared scope reference is taken here.
    let Some(scope) = (unsafe { env_scope_mut(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `js_object` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(js_object) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(target) = v8::Local::<v8::Object>::try_from(local) else {
        return NAPI_OBJECT_EXPECTED;
    };
    let Some(key) = wrap_key(env, scope) else {
        return NAPI_GENERIC_FAILURE;
    };

    // The one borrow of the env this function takes. It stays live across
    // the V8 calls below (weak handles, the private property): they allocate
    // but run no JS, and the collector never touches a `NapiEnv` (see
    // `NapiEnv::collected`).
    //
    // "Run no JS", precisely (v8 150.0.0, src/api/api.cc):
    // * `get_private` (in `wrap_id`) is v8::Object::Get under a
    //   PrepareForExecutionScope, and `set_private` runs under an
    //   EnterV8NoScriptScope: both are `CallDepthScope<false>`, which does
    //   not fire V8's call-completed hook. The weak-handle calls enter no
    //   call-depth scope. `wrap_id`'s `number_value` WOULD arm the hook
    //   (`CallDepthScope<true>`) on its slow path, and never takes it: it
    //   is reached only behind `is_number()`, which V8 answers before
    //   entering any scope. A private symbol has no accessor and is not
    //   routed through a Proxy trap, so the property access itself calls
    //   nothing.
    // * Those are V8 internals. What this runtime guarantees is that the
    //   hook would run nothing even if one of these calls armed it:
    //   `JsRuntime::new` sets `MicrotasksPolicy::Explicit` (lib.rs) and
    //   installs no call-completed callback or custom microtask queue.
    //   Under `MicrotasksPolicy::Auto` a hook-arming call runs a microtask
    //   checkpoint on leaving API call depth zero, and a promise reaction
    //   there may call into the addon. See `napi_create_function`.
    // SAFETY: `env` is non-null and live (env_scope_mut found its scope); no other reference into it is live, and nothing below re-enters this ABI (see the account above).
    let env_ref = unsafe { &mut *env };

    // The optional result is a WEAK reference to the object (count 0), as in
    // Node: usable with the reference API, and not something that keeps the
    // wrapped object alive. Minted first, so a full reference table refuses
    // the call before anything is installed.
    let mut minted: NapiRefHandle = std::ptr::null_mut();
    if !result.is_null() {
        match env_ref.mint_weak_ref(scope, local) {
            Some(handle) => minted = handle,
            None => return NAPI_GENERIC_FAILURE,
        }
    }

    // Replace any existing wrap for this object: the entry keeps its weak
    // handle and id, and takes the new pointer and finalizer.
    let mut replaced = false;
    if let Some(id) = wrap_id(scope, target, key)
        && let Some(existing) = env_ref.tracked.get_mut(&id)
    {
        existing.native = native_object;
        existing.finalize = finalize_cb;
        existing.hint = finalize_hint;
        replaced = true;
    }
    if !replaced {
        let id = env_ref.track_weak(scope, local, native_object, finalize_cb, finalize_hint);
        let id_value = v8::Number::new(scope, id as f64);
        if target.set_private(scope, key, id_value.into()) != Some(true) {
            // The object could not be marked, so the wrap would be
            // unreachable: take back the entry and the reference. A null
            // `minted` decodes to a miss.
            env_ref.tracked.remove(&id);
            env_ref.free_ref(minted);
            return NAPI_GENERIC_FAILURE;
        }
    }

    if !result.is_null() {
        // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *result = minted };
    }
    NAPI_OK
}

/// Retrieve the native pointer stored by `napi_wrap`.
/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_unwrap(
    env: Env,
    js_object: NapiValue,
    result: *mut *mut c_void,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `js_object` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(js_object) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(target) = v8::Local::<v8::Object>::try_from(local) else {
        return NAPI_OBJECT_EXPECTED;
    };
    // One private-property read and one map lookup, whatever the number of
    // live wraps.
    let Some(id) = wrap_key(env, scope).and_then(|key| wrap_id(scope, target, key)) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `env` is the caller's `*mut NapiEnv`, proved non-null by env_scope; the shared reborrow ends with this statement, which does not call into V8.
    let Some(native) = unsafe { &*env }.tracked.get(&id).map(|entry| entry.native) else {
        return NAPI_INVALID_ARG;
    };
    if !result.is_null() {
        // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *result = native };
    }
    NAPI_OK
}

/// Remove the wrap stored by `napi_wrap` and retrieve the native pointer.
/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_remove_wrap(
    env: Env,
    js_object: NapiValue,
    result: *mut *mut c_void,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `js_object` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(js_object) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(target) = v8::Local::<v8::Object>::try_from(local) else {
        return NAPI_OBJECT_EXPECTED;
    };
    let Some(key) = wrap_key(env, scope) else {
        return NAPI_INVALID_ARG;
    };
    let Some(id) = wrap_id(scope, target, key) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `env` is the caller's `*mut NapiEnv`, proved non-null by env_scope; the reborrow ends with this statement, which does not call into V8.
    let Some(entry) = unsafe { &mut *env }.tracked.remove(&id) else {
        return NAPI_INVALID_ARG;
    };
    let native = entry.native;
    // Releasing the weak handle unregisters its V8 finalizer -- a removed
    // wrap's finalizer never runs, as in Node.
    drop(entry);
    target.delete_private(scope, key);
    if !result.is_null() {
        // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *result = native };
    }
    NAPI_OK
}

/// Call a constructor function with `new` to produce a new instance.
/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_new_instance(
    env: Env,
    constructor: NapiValue,
    argc: usize,
    argv: *const NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `constructor` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(ctor_local) = (unsafe { to_local(constructor) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(ctor_fn) = v8::Local::<v8::Function>::try_from(ctor_local) else {
        return NAPI_FUNCTION_EXPECTED;
    };
    // As in napi_call_function: a null `argv` with a non-zero count, or a
    // null element, is refused rather than skipped.
    if argc > 0 && argv.is_null() {
        return NAPI_INVALID_ARG;
    }
    let Some(args) = (0..argc)
        // SAFETY: `argv` is non-null whenever `argc` is non-zero (checked above) and points to at least `argc` caller-owned napi_value handles; to_local reinterprets each as a repr-compatible `v8::Local` and returns None for a null handle.
        .map(|i| unsafe { to_local(*argv.add(i)) })
        .collect::<Option<Vec<_>>>()
    else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `env` is the FFI caller's env pointer (catching null-guards it and reports a missing native scope); no other scope reference is taken in this function.
    // `result` is the caller's out-parameter pointer, null-checked before the write.
    unsafe {
        catching_value(env, result, |scope| {
            ctor_fn
                .new_instance(scope, &args)
                .map(|obj| from_local(obj.into()))
        })
    }
}

/// Check `value instanceof constructor`.
/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_instanceof(
    env: Env,
    object: NapiValue,
    constructor: NapiValue,
    result: *mut bool,
) -> NapiStatus {
    // SAFETY: `object` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(obj_local) = (unsafe { to_local(object) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `constructor` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(ctor_local) = (unsafe { to_local(constructor) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(ctor_obj) = v8::Local::<v8::Object>::try_from(ctor_local) else {
        return NAPI_FUNCTION_EXPECTED;
    };
    // SAFETY: `env` is the FFI caller's env pointer (catching null-guards it and reports a missing native scope); no other scope reference is taken in this function.
    match unsafe { catching(env, |scope| obj_local.instance_of(scope, ctor_obj)) } {
        // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
        Ok(is) => unsafe { out(result, is) },
        Err(status) => status,
    }
}

// =================================================================== bigint

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_bigint_int64(
    env: Env,
    value: i64,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe {
        out(
            result,
            from_local(v8::BigInt::new_from_i64(scope, value).into()),
        )
    }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_bigint_uint64(
    env: Env,
    value: u64,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe {
        out(
            result,
            from_local(v8::BigInt::new_from_u64(scope, value).into()),
        )
    }
}

/// Returns `(value, lossless)` where `lossless` is false if the BigInt
/// is too large to represent exactly as i64.
/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_value_bigint_int64(
    env: Env,
    value: NapiValue,
    result: *mut i64,
    lossless: *mut bool,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(bigint) = v8::Local::<v8::BigInt>::try_from(local) else {
        return NAPI_INVALID_ARG;
    };
    let (val, ok) = bigint.i64_value();
    if !result.is_null() {
        // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *result = val };
    }
    if !lossless.is_null() {
        // SAFETY: `lossless` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *lossless = ok };
    }
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_value_bigint_uint64(
    env: Env,
    value: NapiValue,
    result: *mut u64,
    lossless: *mut bool,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(bigint) = v8::Local::<v8::BigInt>::try_from(local) else {
        return NAPI_INVALID_ARG;
    };
    let (val, ok) = bigint.u64_value();
    if !result.is_null() {
        // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *result = val };
    }
    if !lossless.is_null() {
        // SAFETY: `lossless` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *lossless = ok };
    }
    NAPI_OK
}

// =================================================================== buffers

/// Create a zero-initialized Buffer of `size` bytes. Returns a Uint8Array
/// view backed by an ArrayBuffer, plus a pointer to the raw data.
///
/// The returned pointer is valid as long as the V8 heap owns the backing store
/// (i.e. while the ArrayBuffer returned in `result` is alive).
/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_buffer(
    env: Env,
    size: usize,
    data: *mut *mut c_void,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let ab = v8::ArrayBuffer::new(scope, size);
    if !data.is_null() {
        let store = ab.get_backing_store();
        let ptr = store
            .data()
            .map(|p| p.as_ptr())
            .unwrap_or(std::ptr::null_mut());
        // SAFETY: `data` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *data = ptr };
    }
    let Some(view) = v8::Uint8Array::new(scope, ab, 0, size) else {
        return NAPI_GENERIC_FAILURE;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(view.into())) }
}

/// Create a Buffer, copy `size` bytes from `data` into it.
/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_buffer_copy(
    env: Env,
    size: usize,
    data: *const c_void,
    result_data: *mut *mut c_void,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let ab = v8::ArrayBuffer::new(scope, size);
    let store = ab.get_backing_store();
    if let Some(dst) = store.data() {
        // SAFETY: `data` points to at least `size` caller-owned elements; from_raw_parts reads them for the length the C caller declared.
        let src_bytes = unsafe { std::slice::from_raw_parts(data as *const u8, size) };
        // SAFETY: `dst.as_ptr` points to at least `size` caller-owned writable elements; from_raw_parts_mut forms the out-slice written below.
        let dst_bytes = unsafe { std::slice::from_raw_parts_mut(dst.as_ptr() as *mut u8, size) };
        dst_bytes.copy_from_slice(src_bytes);
        if !result_data.is_null() {
            // SAFETY: `result_data` is a caller-provided out-parameter pointer, null-checked before this write.
            unsafe { *result_data = dst.as_ptr() };
        }
    }
    let Some(view) = v8::Uint8Array::new(scope, ab, 0, size) else {
        return NAPI_GENERIC_FAILURE;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(view.into())) }
}

/// Deleter for a backing store that aliases addon-owned memory. Deliberately
/// empty: V8 may release a backing store from a background thread, and the
/// memory is the addon's to release, on the JS thread, through the
/// `napi_finalize` it supplied -- which the weak tracking in
/// [`external_arraybuffer`] arranges.
extern "C" fn external_store_deleter(_data: *mut c_void, _len: usize, _hint: *mut c_void) {}

/// An ArrayBuffer over the addon's own memory: no copy, so a later native
/// write is visible to JS and the reverse. When the ArrayBuffer is collected
/// the addon's `finalize(env, data, hint)` runs the way a wrap's does (see
/// [`NapiWrapEntry`]); until then the memory must stay valid.
///
/// A zero length is an ordinary empty ArrayBuffer -- there is nothing to
/// alias, and `data` is not read -- but the finalizer is still owed. `None`
/// when no native scope is installed, or when `data` is null with a non-zero
/// length.
///
/// The ArrayBuffer comes back with the exclusive scope it was made in, so a
/// caller that goes on to build a view over it continues with that one
/// borrow instead of deriving a second scope reference from the env.
///
/// The buffer is given a detach key. Detaching it (`transfer()`, or handing
/// it to another thread) would move the memory to an object this env does
/// not track, so the finalizer could release memory still in use; with the
/// key set those operations are refused instead.
///
/// # Safety
///
/// `env` must be the live `napi_env` this engine handed the addon, or null;
/// `data` must be valid for reads and writes of `byte_length` bytes until
/// `finalize` has run (for the life of the process when there is none). The
/// caller must hold no other scope reference: the one returned is the
/// exclusive one (see [`env_scope_mut`]).
unsafe fn external_arraybuffer<'a>(
    env: Env,
    data: *mut c_void,
    byte_length: usize,
    finalize: Option<NapiFinalize>,
    hint: *mut c_void,
) -> Option<(
    &'a mut v8::PinScope<'static, 'static>,
    v8::Local<'static, v8::ArrayBuffer>,
)> {
    // SAFETY: `env` is the caller's env pointer or null (null-guarded inside); exclusivity of the returned scope is this function's own contract.
    let scope = unsafe { env_scope_mut(env) }?;
    let ab = if byte_length == 0 {
        v8::ArrayBuffer::new(scope, 0)
    } else {
        if data.is_null() {
            return None;
        }
        // SAFETY: `data` is non-null (checked above) and valid for `byte_length` bytes for as long as the store can be reached, per this function's contract; the deleter is a no-op, so V8 never releases the addon's memory itself.
        let store = unsafe {
            v8::ArrayBuffer::new_backing_store_from_ptr(
                data,
                byte_length,
                external_store_deleter,
                std::ptr::null_mut(),
            )
        };
        let ab = v8::ArrayBuffer::with_backing_store(scope, &store.make_shared());
        if let Some(key) = v8::String::new(scope, "oam:napi:external") {
            ab.set_detach_key(key.into());
        }
        ab
    };
    if finalize.is_some() {
        // SAFETY: `env` is the live env `scope` was taken from (non-null, or there would be no scope); no other reference into it is live, and the borrow ends with this statement, which runs no JS.
        unsafe { &mut *env }.track_weak(scope, ab.into(), data, finalize, hint);
    }
    Some((scope, ab))
}

/// Create an external buffer: a Uint8Array over an ArrayBuffer that aliases
/// the addon's memory (see [`external_arraybuffer`]). The finalizer runs
/// after the ArrayBuffer has been collected.
/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_external_buffer(
    env: Env,
    size: usize,
    data: *mut c_void,
    finalize_cb: Option<NapiFinalize>,
    finalize_hint: *mut c_void,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's env pointer (null-guarded inside); `data` is the addon's memory of `size` bytes, valid until its finalizer runs per the Node-API contract; this function takes no scope reference of its own.
    let Some((scope, ab)) =
        (unsafe { external_arraybuffer(env, data, size, finalize_cb, finalize_hint) })
    else {
        return NAPI_INVALID_ARG;
    };
    let Some(view) = v8::Uint8Array::new(scope, ab, 0, size) else {
        return NAPI_GENERIC_FAILURE;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(view.into())) }
}

/// Returns true for a `Uint8Array` -- which a `Buffer` is -- and false for
/// every other value, including the other TypedArray kinds and DataView
/// (the test is `is_uint8_array()`). Addons that pass a plain Uint8Array
/// or a Buffer both work, which is the relevant use case.
/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_is_buffer(
    env: Env,
    value: NapiValue,
    result: *mut bool,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, local.is_uint8_array()) }
}

/// Get a pointer to the underlying bytes of a TypedArray or ArrayBuffer.
/// For a TypedArray, accounts for byte_offset so `*data` points to element 0.
/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_buffer_info(
    env: Env,
    value: NapiValue,
    data: *mut *mut c_void,
    length: *mut usize,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };

    if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(local) {
        let ab = match view.buffer(scope) {
            Some(ab) => ab,
            None => return NAPI_GENERIC_FAILURE,
        };
        let store = ab.get_backing_store();
        let byte_off = view.byte_offset();
        let byte_len = view.byte_length();
        if !data.is_null() {
            let base = store
                .data()
                .map(|p| p.as_ptr() as *mut u8)
                .unwrap_or(std::ptr::null_mut());
            // SAFETY: `data` is a caller-provided out-parameter pointer, null-checked before this write.
            unsafe { *data = base.add(byte_off) as *mut c_void };
        }
        if !length.is_null() {
            // SAFETY: `length` is a caller-provided out-parameter pointer, null-checked before this write.
            unsafe { *length = byte_len };
        }
        NAPI_OK
    } else if let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(local) {
        let store = ab.get_backing_store();
        if !data.is_null() {
            let ptr = store
                .data()
                .map(|p| p.as_ptr())
                .unwrap_or(std::ptr::null_mut());
            // SAFETY: `data` is a caller-provided out-parameter pointer, null-checked before this write.
            unsafe { *data = ptr };
        }
        if !length.is_null() {
            // SAFETY: `length` is a caller-provided out-parameter pointer, null-checked before this write.
            unsafe { *length = ab.byte_length() };
        }
        NAPI_OK
    } else {
        NAPI_INVALID_ARG
    }
}

// ======== gamma wave: full napi-sys-2.4.0 surface ========

/// Extended error info returned by napi_get_last_error_info.
#[repr(C)]
pub struct NapiExtendedErrorInfo {
    pub error_message: *const c_char,
    pub engine_reserved: *mut c_void,
    pub engine_error_code: u32,
    pub error_code: NapiStatus,
}

/// Node.js version info returned by napi_get_node_version.
#[repr(C)]
pub struct NapiNodeVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    pub release: *const c_char,
}
// SAFETY: only ever read from C side; never mutated after initialisation.
unsafe impl Sync for NapiNodeVersion {}

static NODE_VERSION: NapiNodeVersion = NapiNodeVersion {
    major: 22,
    minor: 0,
    patch: 0,
    release: c"node".as_ptr(),
};

// uv_event_loop: a data symbol addons may look up via GetProcAddress.
// Exported as a null pointer (oam has no libuv event loop).
#[unsafe(no_mangle)]
pub static mut uv_event_loop: *mut c_void = std::ptr::null_mut();

// ------------------------------------------------------------------ errors

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_last_error_info(
    env: Env,
    result: *mut *const NapiExtendedErrorInfo,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; `as_ref` guards null and
    // the shared reborrow only reads the env's own last-error slot.
    let Some(env_ref) = (unsafe { env.as_ref() }) else {
        return NAPI_INVALID_ARG;
    };
    if !result.is_null() {
        // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked
        // before this write. The pointer written borrows the env's own slot, which
        // outlives this call -- the addon is calling THROUGH that env.
        unsafe { *result = &raw const env_ref.last_error };
    }
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_error(
    env: Env,
    code: NapiValue,
    msg: NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: the caller's arguments, passed straight through under this entry point's own contract.
    unsafe { create_error_with(env, code, msg, result, v8::Exception::error) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_type_error(
    env: Env,
    code: NapiValue,
    msg: NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: the caller's arguments, passed straight through under this entry point's own contract.
    unsafe { create_error_with(env, code, msg, result, v8::Exception::type_error) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_range_error(
    env: Env,
    code: NapiValue,
    msg: NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: the caller's arguments, passed straight through under this entry point's own contract.
    unsafe { create_error_with(env, code, msg, result, v8::Exception::range_error) }
}

/// The body of napi_create_error / _type_error / _range_error, which differ
/// only in the V8 constructor. As in Node: `msg` must be a string
/// (`napi_string_expected`), a null `code` adds no property, and a `code`
/// that is not a string is `napi_string_expected`.
///
/// # Safety
///
/// The entry points' contract: `env` must be the live `napi_env` this engine
/// handed the addon (or null), with a native entry on the stack; `msg` must
/// be a `napi_value` from it and `code` one or null; `result` must be null
/// or valid for a write of one `napi_value`.
unsafe fn create_error_with(
    env: Env,
    code: NapiValue,
    msg: NapiValue,
    result: *mut NapiValue,
    constructor: for<'s> fn(
        &v8::PinScope<'s, '_>,
        v8::Local<v8::String>,
    ) -> v8::Local<'s, v8::Value>,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `msg` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(msg_local) = (unsafe { to_local(msg) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(msg_str) = v8::Local::<v8::String>::try_from(msg_local) else {
        return NAPI_STRING_EXPECTED;
    };
    let error = constructor(scope, msg_str);
    // SAFETY: `code` is null or a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    if let Some(code) = unsafe { to_local(code) } {
        if !code.is_string() {
            return NAPI_STRING_EXPECTED;
        }
        let Ok(object) = v8::Local::<v8::Object>::try_from(error) else {
            return NAPI_GENERIC_FAILURE;
        };
        let Some(key) = v8::String::new(scope, "code") else {
            return NAPI_GENERIC_FAILURE;
        };
        if object.set(scope, key.into(), code) != Some(true) {
            return NAPI_GENERIC_FAILURE;
        }
    }
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(error)) }
}

/// A RangeError carrying `code`, ready for an env's deferred-exception slot:
/// what the argument checks Node reports that way (typed-array and DataView
/// construction) leave pending. Built from this file's own text, so nothing
/// here reads a caller's pointer. `None` when V8 could not build it.
fn coded_range_error(
    scope: &v8::PinScope<'_, '_>,
    code: &str,
    message: &str,
) -> Option<v8::Global<v8::Value>> {
    let message = v8::String::new(scope, message)?;
    let error = v8::Exception::range_error(scope, message);
    if let Ok(object) = v8::Local::<v8::Object>::try_from(error) {
        let key = v8::String::new(scope, "code")?;
        if let Some(value) = v8::String::new(scope, code) {
            object.set(scope, key.into(), value.into());
        }
    }
    Some(v8::Global::new(scope, error))
}

fn range_error<'s>(
    scope: &v8::PinScope<'s, '_>,
    message: v8::Local<v8::String>,
) -> v8::Local<'static, v8::Value> {
    let error = v8::Exception::range_error(scope, message);
    // SAFETY: relifetimes the freshly built error `v8::Local` to 'static; build_error immediately re-roots it into a `v8::Global` before the scope ends, so the handle stays valid.
    unsafe { std::mem::transmute::<v8::Local<v8::Value>, v8::Local<'static, v8::Value>>(error) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_throw_range_error(
    env: Env,
    code: *const c_char,
    msg: *const c_char,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `code` and `msg` are the caller's optional C strings; build_error reads each only after a null check.
    match unsafe { build_error(scope, code, msg, range_error) } {
        Some(error) => {
            // SAFETY: `env` is a valid `*mut NapiEnv` (established above); reads/writes its `pending` deferred-exception slot.
            unsafe { (*env).pending = Some(error) };
            NAPI_OK
        }
        None => NAPI_GENERIC_FAILURE,
    }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_is_error(
    env: Env,
    value: NapiValue,
    result: *mut bool,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    // An Error is an object; check by seeing if it has a 'message' property
    // and is an instance of Error via the global Error constructor.
    let is_err = if let Ok(obj) = v8::Local::<v8::Object>::try_from(local) {
        let ctx = scope.get_current_context();
        let global = ctx.global(scope);
        let error_key = v8::String::new(scope, "Error").unwrap();
        if let Some(error_ctor) = global.get(scope, error_key.into()) {
            if let Ok(ctor_obj) = v8::Local::<v8::Object>::try_from(error_ctor) {
                obj.instance_of(scope, ctor_obj).unwrap_or(false)
            } else {
                false
            }
        } else {
            false
        }
    } else {
        false
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, is_err) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_fatal_error(
    location: *const c_char,
    location_len: usize,
    message: *const c_char,
    message_len: usize,
) -> ! {
    // SAFETY: `location` is null, or the caller's string of `location_len` bytes (NUL-terminated when the length is NAPI_AUTO_LENGTH); sized_text null-checks it.
    let loc =
        unsafe { sized_text(location, location_len) }.unwrap_or_else(|| "<unknown>".to_string());
    // SAFETY: `message` is null, or the caller's string of `message_len` bytes (NUL-terminated when the length is NAPI_AUTO_LENGTH); sized_text null-checks it.
    let msg =
        unsafe { sized_text(message, message_len) }.unwrap_or_else(|| "<no message>".to_string());
    eprintln!("FATAL ERROR: {loc}: {msg}");
    std::process::abort()
}

/// Read a caller string given as pointer + length, where a length of
/// `usize::MAX` (NAPI_AUTO_LENGTH) means "NUL-terminated". An explicit
/// length is honoured exactly: such a string need not be NUL-terminated, so
/// scanning for a terminator would read past its end. `None` for a null
/// pointer.
///
/// # Safety
///
/// `text` must be null, or valid for reads of `len` bytes -- or, when `len`
/// is `usize::MAX`, a NUL-terminated C string.
unsafe fn sized_text(text: *const c_char, len: usize) -> Option<String> {
    if text.is_null() {
        return None;
    }
    // SAFETY: `text` is non-null (checked above) and valid for reads of `len` bytes -- or NUL-terminated for NAPI_AUTO_LENGTH -- per this function's contract, which is `napi_text_units`' contract; the slice is dropped before this call returns.
    let bytes = unsafe { napi_text_units(text.cast::<u8>(), len) };
    Some(String::from_utf8_lossy(bytes).into_owned())
}

/// Whether `len` is a length N-API accepts for a UTF-8 name: NAPI_AUTO_LENGTH
/// (`usize::MAX`), or at most `i32::MAX` bytes. Node refuses anything else
/// with `napi_invalid_arg` before reading the string (js_native_api_v8.cc,
/// `CHECK_NEW_FROM_UTF8_LEN`).
fn is_napi_string_length(len: usize) -> bool {
    len == usize::MAX || len <= i32::MAX as usize
}

/// Whether the arguments of a `napi_create_string_*` call pass Node's checks
/// (js_native_api_v8.cc, `NewString`): the text may be null only when the
/// length is 0 (the call then makes ""; NAPI_AUTO_LENGTH is not 0), the
/// result pointer must be non-null, and the length must pass
/// [`is_napi_string_length`]. Each failure is `napi_invalid_arg`, decided
/// before any text is read.
fn is_napi_string_args(text: *const c_void, length: usize, result: *mut NapiValue) -> bool {
    (length == 0 || !text.is_null()) && !result.is_null() && is_napi_string_length(length)
}

/// The code units of a `napi_create_string_*` text: `length` of them, or,
/// for NAPI_AUTO_LENGTH (`usize::MAX`), those before the first 0 unit. A
/// null `text` with length 0 is the empty slice.
///
/// # Safety
///
/// `text` must be null with `length` 0, or valid for reads of `length` units
/// -- or, when `length` is `usize::MAX`, of every unit up to and including a
/// 0 unit. The returned slice borrows that memory for the caller-chosen `'a`,
/// which must not outlive the call the text was passed to.
unsafe fn napi_text_units<'a, T: Copy + Default + PartialEq>(
    text: *const T,
    length: usize,
) -> &'a [T] {
    if text.is_null() || length == 0 {
        return &[];
    }
    let len = if length == usize::MAX {
        let mut len = 0usize;
        // SAFETY: `text` is 0-terminated for NAPI_AUTO_LENGTH per this function's contract, so every unit up to the terminator is readable.
        while unsafe { *text.add(len) } != T::default() {
            len += 1;
        }
        len
    } else {
        length
    };
    // SAFETY: `text` is non-null and valid for reads of `len` units per this function's contract (counted above for NAPI_AUTO_LENGTH).
    unsafe { std::slice::from_raw_parts(text, len) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_fatal_exception(env: Env, _err: NapiValue) -> NapiStatus {
    let _ = env;
    NAPI_GENERIC_FAILURE
}

// ------------------------------------------------------------------ strings

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_string_latin1(
    env: Env,
    str_ptr: *const c_char,
    length: usize,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    if !is_napi_string_args(str_ptr.cast(), length, result) {
        return NAPI_INVALID_ARG;
    }
    // SAFETY: `str_ptr` is null only with length 0 (checked above), and otherwise the caller's text of `length` bytes, NUL-terminated when the length is NAPI_AUTO_LENGTH.
    let bytes: &[u8] = unsafe { napi_text_units(str_ptr.cast::<u8>(), length) };
    let Some(s) = v8::String::new_from_one_byte(scope, bytes, v8::NewStringType::Normal) else {
        return NAPI_GENERIC_FAILURE;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(s.into())) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_string_utf16(
    env: Env,
    str_ptr: *const u16,
    length: usize,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    if !is_napi_string_args(str_ptr.cast(), length, result) {
        return NAPI_INVALID_ARG;
    }
    // SAFETY: `str_ptr` is null only with length 0 (checked above), and otherwise the caller's text of `length` code units, 0-terminated when the length is NAPI_AUTO_LENGTH.
    let units: &[u16] = unsafe { napi_text_units(str_ptr, length) };
    let Some(s) = v8::String::new_from_two_byte(scope, units, v8::NewStringType::Normal) else {
        return NAPI_GENERIC_FAILURE;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(s.into())) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_value_string_latin1(
    env: Env,
    value: NapiValue,
    buf: *mut c_char,
    bufsize: usize,
    result: *mut usize,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(string) = v8::Local::<v8::String>::try_from(local) else {
        return NAPI_STRING_EXPECTED;
    };
    // Latin-1 length = number of UTF-16 code units (chars) for BMP strings
    let char_len = string.length();
    if buf.is_null() {
        // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
        return unsafe { out(result, char_len) };
    }
    if bufsize == 0 {
        // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
        return unsafe { out(result, 0) };
    }
    // Write latin-1 bytes via write_one_byte_v2.
    let copy_len = char_len.min(bufsize - 1);
    // SAFETY: `buf` points to at least `copy_len` caller-owned writable elements; from_raw_parts_mut forms the out-slice written below.
    let buf_slice = unsafe { std::slice::from_raw_parts_mut(buf.cast::<u8>(), copy_len) };
    string.write_one_byte_v2(scope, 0, buf_slice, v8::WriteFlags::empty());
    // SAFETY: writes the trailing NUL into the caller's `buf` at the copy length (within the `bufsize` bound checked above).
    unsafe { *(buf.add(copy_len)) = 0 };
    if !result.is_null() {
        // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *result = copy_len };
    }
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_value_string_utf16(
    env: Env,
    value: NapiValue,
    buf: *mut u16,
    bufsize: usize,
    result: *mut usize,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(string) = v8::Local::<v8::String>::try_from(local) else {
        return NAPI_STRING_EXPECTED;
    };
    let char_len = string.length();
    if buf.is_null() {
        // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
        return unsafe { out(result, char_len) };
    }
    if bufsize == 0 {
        // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
        return unsafe { out(result, 0) };
    }
    let copy_len = char_len.min(bufsize - 1);
    // SAFETY: `buf` points to at least `copy_len` caller-owned writable elements; from_raw_parts_mut forms the out-slice written below.
    let buf_slice = unsafe { std::slice::from_raw_parts_mut(buf, copy_len) };
    string.write_v2(scope, 0, buf_slice, v8::WriteFlags::empty());
    // SAFETY: writes the trailing NUL into the caller's `buf` at the copy length (within the `bufsize` bound checked above).
    unsafe { *(buf.add(copy_len)) = 0u16 };
    if !result.is_null() {
        // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *result = copy_len };
    }
    NAPI_OK
}

// ------------------------------------------------------------------ symbols

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_symbol(
    env: Env,
    description: NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let desc = if description.is_null() {
        None
    } else {
        // SAFETY: `description` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
        unsafe { to_local(description) }.and_then(|v| v8::Local::<v8::String>::try_from(v).ok())
    };
    let sym = v8::Symbol::new(scope, desc);
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(sym.into())) }
}

// ------------------------------------------------------------------ handle scopes (stubs)

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_open_handle_scope(_env: Env, result: *mut *mut c_void) -> NapiStatus {
    // We don't implement real handle scopes -- the V8 PinScope on the stack
    // serves the same purpose. Return a sentinel so callers don't null-deref.
    if !result.is_null() {
        // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *result = std::ptr::dangling_mut::<c_void>() };
    }
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_close_handle_scope(_env: Env, _scope: *mut c_void) -> NapiStatus {
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_open_escapable_handle_scope(
    _env: Env,
    result: *mut *mut c_void,
) -> NapiStatus {
    if !result.is_null() {
        // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *result = std::ptr::dangling_mut::<c_void>() };
    }
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_close_escapable_handle_scope(
    _env: Env,
    _scope: *mut c_void,
) -> NapiStatus {
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_escape_handle(
    _env: Env,
    _scope: *mut c_void,
    escapee: NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    // No real escapable scope needed; just forward the value.
    if result.is_null() {
        return NAPI_INVALID_ARG;
    }
    // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
    unsafe { *result = escapee };
    NAPI_OK
}

// ------------------------------------------------------------------ callback scope stubs

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_open_callback_scope(
    _env: Env,
    _resource_object: NapiValue,
    _context: *mut c_void,
    result: *mut *mut c_void,
) -> NapiStatus {
    if !result.is_null() {
        // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *result = std::ptr::dangling_mut::<c_void>() };
    }
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_close_callback_scope(_env: Env, _scope: *mut c_void) -> NapiStatus {
    NAPI_OK
}

// ------------------------------------------------------------------ properties

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_has_property(
    env: Env,
    object: NapiValue,
    key: NapiValue,
    result: *mut bool,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `object` is a napi_value handle from this env; as_object reads it as a `v8::Local` and downcasts to Object (None when null or not an object).
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    // SAFETY: `key` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(key_local) = (unsafe { to_local(key) }) else {
        return NAPI_INVALID_ARG;
    };
    let has = target.has(scope, key_local).unwrap_or(false);
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, has) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_delete_property(
    env: Env,
    object: NapiValue,
    key: NapiValue,
    result: *mut bool,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `object` is a napi_value handle from this env; as_object reads it as a `v8::Local` and downcasts to Object (None when null or not an object).
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    // SAFETY: `key` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(key_local) = (unsafe { to_local(key) }) else {
        return NAPI_INVALID_ARG;
    };
    let deleted = target.delete(scope, key_local).unwrap_or(false);
    if !result.is_null() {
        // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *result = deleted };
    }
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_has_own_property(
    env: Env,
    object: NapiValue,
    key: NapiValue,
    result: *mut bool,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `object` is a napi_value handle from this env; as_object reads it as a `v8::Local` and downcasts to Object (None when null or not an object).
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    // SAFETY: `key` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(key_local) = (unsafe { to_local(key) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(key_name) = v8::Local::<v8::Name>::try_from(key_local) else {
        return NAPI_INVALID_ARG;
    };
    let has = target.has_own_property(scope, key_name).unwrap_or(false);
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, has) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_has_element(
    env: Env,
    object: NapiValue,
    index: u32,
    result: *mut bool,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `object` is a napi_value handle from this env; as_object reads it as a `v8::Local` and downcasts to Object (None when null or not an object).
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    let has = target.has_index(scope, index).unwrap_or(false);
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, has) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_delete_element(
    env: Env,
    object: NapiValue,
    index: u32,
    result: *mut bool,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `object` is a napi_value handle from this env; as_object reads it as a `v8::Local` and downcasts to Object (None when null or not an object).
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    let deleted = target.delete_index(scope, index).unwrap_or(false);
    if !result.is_null() {
        // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *result = deleted };
    }
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_define_properties(
    env: Env,
    object: NapiValue,
    count: usize,
    props: *const NapiPropertyDescriptor,
) -> NapiStatus {
    // The exclusive scope, and the only one this function takes: a
    // descriptor's method, getter or setter is built by `create_function`,
    // which tracks its External weakly (`&mut Isolate`).
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv` (null-guarded inside); a native entry is on the stack per this function's contract, and no shared scope reference is taken here.
    let Some(scope) = (unsafe { env_scope_mut(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // As in Node, a property count needs a descriptor array, checked before
    // the target is looked at.
    if count > 0 && props.is_null() {
        return NAPI_INVALID_ARG;
    }
    // SAFETY: `object` is a napi_value handle from this env; as_object reads it as a `v8::Local` and downcasts to Object (None when null or not an object).
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    if count == 0 {
        return NAPI_OK;
    }
    // SAFETY: `props` is non-null (checked above) and points to at least `count` caller-owned elements; from_raw_parts reads them for the length the C caller declared.
    let descriptors = unsafe { std::slice::from_raw_parts(props, count) };
    for prop in descriptors {
        // SAFETY: `env` is the caller's live env (env_scope_mut proved it) with no reference into it held here; `scope` is this entry point's one scope reference; `prop` is one of the caller's descriptors, whose name, value and callbacks are the addon's own per this entry point's contract.
        unsafe { define_descriptor(env, scope, target, prop, MethodNaming::Unnamed) };
    }
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_property_names(
    env: Env,
    object: NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `object` is a napi_value handle from this env; as_object reads it as a `v8::Local` and downcasts to Object (None when null or not an object).
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    // SAFETY: `env` is the FFI caller's env pointer (catching null-guards it and reports a missing native scope); no other scope reference is taken in this function.
    // `result` is the caller's out-parameter pointer, null-checked before the write.
    unsafe {
        catching_value(env, result, |scope| {
            target
                .get_property_names(scope, v8::GetPropertyNamesArgs::default())
                .map(|names| from_local(names.into()))
        })
    }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_prototype(
    env: Env,
    object: NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `object` is a napi_value handle from this env; as_object reads it as a `v8::Local` and downcasts to Object (None when null or not an object).
    let Some(target) = (unsafe { as_object(object) }) else {
        return NAPI_OBJECT_EXPECTED;
    };
    let proto = target.get_prototype(scope);
    match proto {
        // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
        Some(p) => unsafe { out(result, from_local(p)) },
        // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
        None => unsafe { out(result, from_local(v8::null(scope).into())) },
    }
}

// ------------------------------------------------------------------ callbacks

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_new_target(
    _env: Env,
    cbinfo: *mut CbInfo,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `cbinfo` is the CbInfo pointer the trampoline placed on its stack and passed to this callback; as_ref yields None if the addon passed null.
    let Some(info) = (unsafe { cbinfo.as_ref() }) else {
        return NAPI_INVALID_ARG;
    };
    // `new.target` for a construct call; a null napi_value for a plain call,
    // which is how Node-API says "not called with `new`".
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, info.new_target) }
}

// ------------------------------------------------------------------ coercions

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_coerce_to_bool(
    env: Env,
    value: NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let b = local.to_boolean(scope);
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(b.into())) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_coerce_to_number(
    env: Env,
    value: NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `env` is the FFI caller's env pointer (catching null-guards it and reports a missing native scope); no other scope reference is taken in this function.
    // `result` is the caller's out-parameter pointer, null-checked before the write.
    unsafe {
        catching_value(env, result, |scope| {
            local.to_number(scope).map(|n| from_local(n.into()))
        })
    }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_coerce_to_object(
    env: Env,
    value: NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `env` is the FFI caller's env pointer (catching null-guards it and reports a missing native scope); no other scope reference is taken in this function.
    // `result` is the caller's out-parameter pointer, null-checked before the write.
    unsafe {
        catching_value(env, result, |scope| {
            local.to_object(scope).map(|obj| from_local(obj.into()))
        })
    }
}

// ------------------------------------------------------------------ arraybuffers

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_is_arraybuffer(
    env: Env,
    value: NapiValue,
    result: *mut bool,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, local.is_array_buffer()) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_arraybuffer(
    env: Env,
    size: usize,
    data: *mut *mut c_void,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let ab = v8::ArrayBuffer::new(scope, size);
    if !data.is_null() {
        let store = ab.get_backing_store();
        let ptr = store
            .data()
            .map(|p| p.as_ptr())
            .unwrap_or(std::ptr::null_mut());
        // SAFETY: `data` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *data = ptr };
    }
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(ab.into())) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_external_arraybuffer(
    env: Env,
    external_data: *mut c_void,
    byte_length: usize,
    finalize_cb: Option<NapiFinalize>,
    finalize_hint: *mut c_void,
    result: *mut NapiValue,
) -> NapiStatus {
    // The ArrayBuffer aliases the addon's memory and its finalizer runs
    // after the ArrayBuffer has been collected (see external_arraybuffer).
    // SAFETY: `env` is the FFI caller's env pointer (null-guarded inside); `external_data` is the addon's memory of `byte_length` bytes, valid until its finalizer runs per the Node-API contract; this function takes no scope reference of its own.
    let Some((_, ab)) = (unsafe {
        external_arraybuffer(env, external_data, byte_length, finalize_cb, finalize_hint)
    }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(ab.into())) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_arraybuffer_info(
    env: Env,
    ab_value: NapiValue,
    data: *mut *mut c_void,
    length: *mut usize,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `ab_value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(ab_value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(local) else {
        return NAPI_INVALID_ARG;
    };
    let store = ab.get_backing_store();
    if !data.is_null() {
        // SAFETY: `data` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe {
            *data = store
                .data()
                .map(|p| p.as_ptr())
                .unwrap_or(std::ptr::null_mut())
        };
    }
    if !length.is_null() {
        // SAFETY: `length` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *length = ab.byte_length() };
    }
    NAPI_OK
}

// ------------------------------------------------------------------ typedarrays

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_is_typedarray(
    env: Env,
    value: NapiValue,
    result: *mut bool,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, local.is_typed_array()) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_typedarray(
    env: Env,
    type_: u32,
    count: usize,
    ab_value: NapiValue,
    offset: usize,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `ab_value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(ab_value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(local) else {
        return NAPI_INVALID_ARG;
    };
    // Node validates the view against the ArrayBuffer before constructing it
    // and reports a mismatch as a RangeError with a documented code; V8's
    // constructors take the offset and length unchecked.
    let (element_size, misaligned): (usize, &'static str) = match type_ {
        0..=2 => (1, ""),
        3 => (2, "start offset of Int16Array should be a multiple of 2"),
        4 => (2, "start offset of Uint16Array should be a multiple of 2"),
        5 => (4, "start offset of Int32Array should be a multiple of 4"),
        6 => (4, "start offset of Uint32Array should be a multiple of 4"),
        7 => (4, "start offset of Float32Array should be a multiple of 4"),
        8 => (8, "start offset of Float64Array should be a multiple of 8"),
        9 => (8, "start offset of BigInt64Array should be a multiple of 8"),
        10 => (
            8,
            "start offset of BigUint64Array should be a multiple of 8",
        ),
        _ => return NAPI_INVALID_ARG,
    };
    let fits = count
        .checked_mul(element_size)
        .and_then(|bytes| bytes.checked_add(offset))
        .is_some_and(|end| end <= ab.byte_length());
    // Alignment is reported ahead of length, as in Node.
    let fault = if !offset.is_multiple_of(element_size) {
        Some(("ERR_NAPI_INVALID_TYPEDARRAY_ALIGNMENT", misaligned))
    } else if !fits {
        Some((
            "ERR_NAPI_INVALID_TYPEDARRAY_LENGTH",
            "Invalid typed array length",
        ))
    } else {
        None
    };
    if let Some((code, message)) = fault {
        if let Some(error) = coded_range_error(scope, code, message) {
            // SAFETY: `env` is non-null and live (env_scope found its scope); writes its `pending` deferred-exception slot. No reference into the env is held here.
            unsafe { (*env).pending = Some(error) };
        }
        return NAPI_GENERIC_FAILURE;
    }
    let view: Option<v8::Local<v8::Value>> = match type_ {
        0 => v8::Int8Array::new(scope, ab, offset, count).map(|v| v.into()),
        1 => v8::Uint8Array::new(scope, ab, offset, count).map(|v| v.into()),
        2 => v8::Uint8ClampedArray::new(scope, ab, offset, count).map(|v| v.into()),
        3 => v8::Int16Array::new(scope, ab, offset, count).map(|v| v.into()),
        4 => v8::Uint16Array::new(scope, ab, offset, count).map(|v| v.into()),
        5 => v8::Int32Array::new(scope, ab, offset, count).map(|v| v.into()),
        6 => v8::Uint32Array::new(scope, ab, offset, count).map(|v| v.into()),
        7 => v8::Float32Array::new(scope, ab, offset, count).map(|v| v.into()),
        8 => v8::Float64Array::new(scope, ab, offset, count).map(|v| v.into()),
        9 => v8::BigInt64Array::new(scope, ab, offset, count).map(|v| v.into()),
        10 => v8::BigUint64Array::new(scope, ab, offset, count).map(|v| v.into()),
        _ => return NAPI_INVALID_ARG,
    };
    match view {
        // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
        Some(v) => unsafe { out(result, from_local(v)) },
        None => NAPI_GENERIC_FAILURE,
    }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_typedarray_info(
    env: Env,
    ta_value: NapiValue,
    type_out: *mut u32,
    count_out: *mut usize,
    data_out: *mut *mut c_void,
    ab_out: *mut NapiValue,
    offset_out: *mut usize,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `ta_value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(ta_value) }) else {
        return NAPI_INVALID_ARG;
    };
    // Determine the element type by checking each concrete typed array type.
    let type_id: u32;
    let element_size: usize;
    if let Ok(v) = v8::Local::<v8::Int8Array>::try_from(local) {
        type_id = 0;
        element_size = 1;
        let _ = v;
    } else if let Ok(v) = v8::Local::<v8::Uint8Array>::try_from(local) {
        type_id = 1;
        element_size = 1;
        let _ = v;
    } else if let Ok(v) = v8::Local::<v8::Uint8ClampedArray>::try_from(local) {
        type_id = 2;
        element_size = 1;
        let _ = v;
    } else if let Ok(v) = v8::Local::<v8::Int16Array>::try_from(local) {
        type_id = 3;
        element_size = 2;
        let _ = v;
    } else if let Ok(v) = v8::Local::<v8::Uint16Array>::try_from(local) {
        type_id = 4;
        element_size = 2;
        let _ = v;
    } else if let Ok(v) = v8::Local::<v8::Int32Array>::try_from(local) {
        type_id = 5;
        element_size = 4;
        let _ = v;
    } else if let Ok(v) = v8::Local::<v8::Uint32Array>::try_from(local) {
        type_id = 6;
        element_size = 4;
        let _ = v;
    } else if let Ok(v) = v8::Local::<v8::Float32Array>::try_from(local) {
        type_id = 7;
        element_size = 4;
        let _ = v;
    } else if let Ok(v) = v8::Local::<v8::Float64Array>::try_from(local) {
        type_id = 8;
        element_size = 8;
        let _ = v;
    } else if let Ok(v) = v8::Local::<v8::BigInt64Array>::try_from(local) {
        type_id = 9;
        element_size = 8;
        let _ = v;
    } else if let Ok(v) = v8::Local::<v8::BigUint64Array>::try_from(local) {
        type_id = 10;
        element_size = 8;
        let _ = v;
    } else {
        return NAPI_INVALID_ARG;
    }
    let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(local) else {
        return NAPI_INVALID_ARG;
    };
    let byte_off = view.byte_offset();
    let byte_len = view.byte_length();
    let elem_count = byte_len.checked_div(element_size).unwrap_or(0);
    if !type_out.is_null() {
        // SAFETY: `type_out` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *type_out = type_id };
    }
    if !count_out.is_null() {
        // SAFETY: `count_out` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *count_out = elem_count };
    }
    if !offset_out.is_null() {
        // SAFETY: `offset_out` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *offset_out = byte_off };
    }
    if let Some(ab) = view.buffer(scope) {
        if !ab_out.is_null() {
            // SAFETY: `ab_out` is a caller-provided out-parameter pointer, null-checked before this write.
            unsafe { *ab_out = from_local(ab.into()) };
        }
        if !data_out.is_null() {
            let store = ab.get_backing_store();
            let base = store
                .data()
                .map(|p| p.as_ptr() as *mut u8)
                .unwrap_or(std::ptr::null_mut());
            // SAFETY: `data_out` is a caller-provided out-parameter pointer, null-checked before this write.
            unsafe { *data_out = base.add(byte_off) as *mut c_void };
        }
    }
    NAPI_OK
}

// ------------------------------------------------------------------ dataview

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_dataview(
    env: Env,
    size: usize,
    ab_value: NapiValue,
    offset: usize,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `ab_value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(ab_value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(local) else {
        return NAPI_INVALID_ARG;
    };
    // As in Node: a view that does not fit the ArrayBuffer is a RangeError
    // with a documented code, left pending, rather than whatever V8's
    // unchecked constructor does with it.
    if !offset
        .checked_add(size)
        .is_some_and(|end| end <= ab.byte_length())
    {
        if let Some(error) = coded_range_error(
            scope,
            "ERR_NAPI_INVALID_DATAVIEW_ARGS",
            "byte_offset + byte_length should be less than or equal to the size in bytes of the array passed in",
        ) {
            // SAFETY: `env` is non-null and live (env_scope found its scope); writes its `pending` deferred-exception slot. No reference into the env is held here.
            unsafe { (*env).pending = Some(error) };
        }
        return NAPI_PENDING_EXCEPTION;
    }
    let dv = v8::DataView::new(scope, ab, offset, size);
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(dv.into())) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_is_dataview(
    env: Env,
    value: NapiValue,
    result: *mut bool,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, local.is_data_view()) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_dataview_info(
    env: Env,
    dv_value: NapiValue,
    byte_length: *mut usize,
    data: *mut *mut c_void,
    ab_out: *mut NapiValue,
    offset: *mut usize,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `dv_value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(dv_value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(local) else {
        return NAPI_INVALID_ARG;
    };
    let byte_off = view.byte_offset();
    let byte_len = view.byte_length();
    if !byte_length.is_null() {
        // SAFETY: `byte_length` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *byte_length = byte_len };
    }
    if !offset.is_null() {
        // SAFETY: `offset` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *offset = byte_off };
    }
    if let Some(ab) = view.buffer(scope) {
        if !ab_out.is_null() {
            // SAFETY: `ab_out` is a caller-provided out-parameter pointer, null-checked before this write.
            unsafe { *ab_out = from_local(ab.into()) };
        }
        if !data.is_null() {
            let store = ab.get_backing_store();
            let base = store
                .data()
                .map(|p| p.as_ptr() as *mut u8)
                .unwrap_or(std::ptr::null_mut());
            // SAFETY: `data` is a caller-provided out-parameter pointer, null-checked before this write.
            unsafe { *data = base.add(byte_off) as *mut c_void };
        }
    }
    NAPI_OK
}

// ------------------------------------------------------------------ promises

/// Heap-allocated resolver, dispensed as an opaque deferred handle.
struct DeferredEntry {
    resolver: v8::Global<v8::PromiseResolver>,
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_promise(
    env: Env,
    deferred: *mut *mut c_void,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(resolver) = v8::PromiseResolver::new(scope) else {
        return NAPI_GENERIC_FAILURE;
    };
    let promise = resolver.get_promise(scope);
    let global_resolver = v8::Global::new(scope, resolver);
    let entry = Box::new(DeferredEntry {
        resolver: global_resolver,
    });
    let ptr = Box::into_raw(entry) as *mut c_void;
    if !deferred.is_null() {
        // SAFETY: `deferred` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *deferred = ptr };
    }
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, from_local(promise.into())) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_resolve_deferred(
    env: Env,
    deferred: *mut c_void,
    resolution: NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    if deferred.is_null() {
        return NAPI_INVALID_ARG;
    }
    // Validate the resolution BEFORE taking ownership of the deferred. N-API
    // frees the deferred only on success; if Box::from_raw ran first, this
    // invalid-arg return would drop (free) the DeferredEntry while the caller
    // still owns the handle, turning a contract-conforming retry into a
    // use-after-free / double-free.
    // SAFETY: `resolution` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(value) = (unsafe { to_local(resolution) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `deferred` is the opaque handle this API handed out via Box::into_raw in napi_create_promise; the C caller passes back the same pointer, so reclaiming the Box is a valid round-trip (non-null and the value validated first).
    let entry = unsafe { Box::from_raw(deferred as *mut DeferredEntry) };
    let resolver = v8::Local::new(scope, &entry.resolver);
    resolver.resolve(scope, value);
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_reject_deferred(
    env: Env,
    deferred: *mut c_void,
    rejection: NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    if deferred.is_null() {
        return NAPI_INVALID_ARG;
    }
    // Validate the rejection BEFORE taking ownership of the deferred (see
    // napi_resolve_deferred): freeing the DeferredEntry on this invalid-arg
    // path would dangle the caller's handle and make a retry a use-after-free.
    // SAFETY: `rejection` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(value) = (unsafe { to_local(rejection) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `deferred` is the opaque handle this API handed out via Box::into_raw in napi_create_promise; the C caller passes back the same pointer, so reclaiming the Box is a valid round-trip (non-null and the value validated first).
    let entry = unsafe { Box::from_raw(deferred as *mut DeferredEntry) };
    let resolver = v8::Local::new(scope, &entry.resolver);
    resolver.reject(scope, value);
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_is_promise(
    env: Env,
    value: NapiValue,
    result: *mut bool,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, local.is_promise()) }
}

// ------------------------------------------------------------------ script

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_run_script(
    env: Env,
    script: NapiValue,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `script` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(script) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(script_str) = v8::Local::<v8::String>::try_from(local) else {
        return NAPI_STRING_EXPECTED;
    };
    // Compile and run under one TryCatch: a SyntaxError and a runtime throw
    // both land in the env's pending-exception slot.
    // SAFETY: `env` is the FFI caller's env pointer (catching null-guards it and reports a missing native scope); no other scope reference is taken in this function.
    let outcome = unsafe {
        catching(env, |scope| {
            let compiled = v8::Script::compile(scope, script_str, None)?;
            compiled.run(scope).map(from_local)
        })
    };
    match outcome {
        Ok(_) if result.is_null() => NAPI_OK,
        // SAFETY: `result` is the caller's out-parameter pointer, non-null on this arm; out() null-checks it again before writing.
        Ok(value) => unsafe { out(result, value) },
        Err(status) => status,
    }
}

// ------------------------------------------------------------------ memory

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_adjust_external_memory(
    _env: Env,
    _change: i64,
    adjusted: *mut i64,
) -> NapiStatus {
    if !adjusted.is_null() {
        // SAFETY: `adjusted` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *adjusted = 0 };
    }
    NAPI_OK
}

// ------------------------------------------------------------------ dates

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_date(
    env: Env,
    time: f64,
    result: *mut NapiValue,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    match v8::Date::new(scope, time) {
        // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
        Some(date) => unsafe { out(result, from_local(date.into())) },
        None => NAPI_GENERIC_FAILURE,
    }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_is_date(env: Env, value: NapiValue, result: *mut bool) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(_scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, local.is_date()) }
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_date_value(
    env: Env,
    value: NapiValue,
    result: *mut f64,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv`; env_scope dereferences it and returns the live `PinScope` stashed on `env.scope` -- non-null only while a native entry (trampoline / load_addon) is on the stack.
    let Some(scope) = (unsafe { env_scope(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `value` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Ok(date) = v8::Local::<v8::Date>::try_from(local) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `result` is the caller's out-parameter pointer; out() null-checks it before writing.
    unsafe { out(result, date.number_value(scope).unwrap_or(f64::NAN)) }
}

// ------------------------------------------------------------------ version / event loop

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_node_version(
    _env: Env,
    version: *mut *const NapiNodeVersion,
) -> NapiStatus {
    if !version.is_null() {
        // SAFETY: `version` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *version = &NODE_VERSION as *const NapiNodeVersion };
    }
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_uv_event_loop(_env: Env, loop_: *mut *mut c_void) -> NapiStatus {
    if !loop_.is_null() {
        // SAFETY: `loop_` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *loop_ = std::ptr::null_mut() };
    }
    NAPI_GENERIC_FAILURE
}

// ------------------------------------------------------------------ module registration

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_module_register(_mod: *mut c_void) -> NapiStatus {
    // Old-style napi_module registration -- not used by napi-sys 2.x.
    NAPI_OK
}

// ------------------------------------------------------------------ cleanup hooks

/// Register `fun(arg)` to run when the env's runtime is torn down. Hooks
/// run in reverse order of registration, before the addon's remaining
/// finalizers and before its library can be unloaded (see [`teardown`]).
/// They do not run when the process ends through `process.exit()`, as in
/// Node.
///
/// A pair that is already registered is refused with `napi_invalid_arg`;
/// Node documents that case as a process abort. So is a registration on an
/// env whose runtime has already been torn down, where the hook could never
/// run.
///
/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// a `napi_env` this engine handed the addon, or null; unlike most entry
/// points it needs no native entry on the stack. `fun` must be null or a
/// function that is safe to call once with `arg` for as long as the addon's
/// library is loaded.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_add_env_cleanup_hook(
    env: Env,
    fun: Option<NapiCleanupHook>,
    arg: *mut c_void,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's env pointer; `as_mut` guards null,
    // and no other reference into the env is live: nothing below runs JS or
    // addon code, so the borrow cannot be re-entered.
    let Some(env_ref) = (unsafe { env.as_mut() }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(fun) = fun else {
        return env_ref.status(
            NAPI_INVALID_ARG,
            Some(c"napi_add_env_cleanup_hook: the hook function is null"),
        );
    };
    if env_ref.closed {
        return env_ref.status(
            NAPI_INVALID_ARG,
            Some(c"napi_add_env_cleanup_hook: the environment has been torn down, the hook would never run"),
        );
    }
    if env_ref.cleanup_hook_position(fun, arg).is_some() {
        return env_ref.status(
            NAPI_INVALID_ARG,
            Some(c"napi_add_env_cleanup_hook: this function and argument are already registered"),
        );
    }
    env_ref.cleanup_hooks.push(CleanupHook {
        seq: next_cleanup_seq(),
        fun,
        arg,
    });
    env_ref.status(NAPI_OK, None)
}

/// Unregister the hook `napi_add_env_cleanup_hook` registered for exactly
/// this `fun` and `arg`. A pair that is not registered -- never was, or has
/// already run -- is left alone and answered `napi_ok`.
///
/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// a `napi_env` this engine handed the addon, or null; it needs no native
/// entry on the stack. `fun` is only compared, never called.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_remove_env_cleanup_hook(
    env: Env,
    fun: Option<NapiCleanupHook>,
    arg: *mut c_void,
) -> NapiStatus {
    // SAFETY: as in `napi_add_env_cleanup_hook` -- null-guarded, and nothing
    // below can re-enter the borrow.
    let Some(env_ref) = (unsafe { env.as_mut() }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(fun) = fun else {
        return env_ref.status(
            NAPI_INVALID_ARG,
            Some(c"napi_remove_env_cleanup_hook: the hook function is null"),
        );
    };
    if let Some(position) = env_ref.cleanup_hook_position(fun, arg) {
        // `remove`, not `swap_remove`: the hooks stay in registration order.
        env_ref.cleanup_hooks.remove(position);
    }
    env_ref.status(NAPI_OK, None)
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_add_finalizer(
    env: Env,
    js_object: NapiValue,
    native_object: *mut c_void,
    finalize_cb: Option<NapiFinalize>,
    finalize_hint: *mut c_void,
    result: *mut NapiRefHandle,
) -> NapiStatus {
    // SAFETY: `env` is the FFI caller's `*mut NapiEnv` (null-guarded inside); a native entry is on the stack per this function's contract, and no shared scope reference is taken here.
    let Some(scope) = (unsafe { env_scope_mut(env) }) else {
        return NAPI_INVALID_ARG;
    };
    // SAFETY: `js_object` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    let Some(local) = (unsafe { to_local(js_object) }) else {
        return NAPI_INVALID_ARG;
    };
    if !local.is_object() {
        return NAPI_OBJECT_EXPECTED;
    }
    if finalize_cb.is_none() {
        return NAPI_INVALID_ARG;
    }
    // The one borrow of the env this function takes; as in napi_wrap, the
    // V8 calls it spans (creating weak handles) allocate but run no JS.
    // In v8 150.0.0 creating a weak handle enters no call-depth scope, so
    // nothing here can reach V8's call-completed hook. That is a V8
    // internal; what this runtime guarantees is that the hook would run
    // nothing anyway: `MicrotasksPolicy::Explicit` (set in `JsRuntime::new`,
    // lib.rs) with no call-completed callback or custom microtask queue
    // installed. Under the auto policy a V8 API call that arms the hook
    // runs a microtask checkpoint on leaving API call depth zero, and a
    // promise reaction could re-enter the addon under this borrow. See
    // `napi_create_function`.
    // SAFETY: `env` is non-null and live (env_scope_mut found its scope); no other reference into it is live, and nothing below re-enters this ABI (see the account above).
    let env_ref = unsafe { &mut *env };
    // The optional result is a weak reference, minted first so a full
    // reference table refuses the call before the finalizer is attached.
    let mut minted: NapiRefHandle = std::ptr::null_mut();
    if !result.is_null() {
        match env_ref.mint_weak_ref(scope, local) {
            Some(handle) => minted = handle,
            None => return NAPI_GENERIC_FAILURE,
        }
    }
    // Tracked like a wrap, but with no id on the object: several finalizers
    // may be attached to one object, and none of them is reachable through
    // napi_unwrap.
    env_ref.track_weak(scope, local, native_object, finalize_cb, finalize_hint);
    if !result.is_null() {
        // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
        unsafe { *result = minted };
    }
    NAPI_OK
}

// ------------------------------------------------------------------ async (stubs -- not supported)

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_async_init(
    _env: Env,
    _resource: NapiValue,
    _name: NapiValue,
    _result: *mut *mut c_void,
) -> NapiStatus {
    NAPI_GENERIC_FAILURE
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_async_destroy(_env: Env, _context: *mut c_void) -> NapiStatus {
    NAPI_GENERIC_FAILURE
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_make_callback(
    _env: Env,
    _context: *mut c_void,
    _recv: NapiValue,
    _func: NapiValue,
    _argc: usize,
    _argv: *const NapiValue,
    _result: *mut NapiValue,
) -> NapiStatus {
    NAPI_GENERIC_FAILURE
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_async_work(
    _env: Env,
    _resource: NapiValue,
    _name: NapiValue,
    _execute: *mut c_void,
    _complete: *mut c_void,
    _data: *mut c_void,
    _result: *mut *mut c_void,
) -> NapiStatus {
    NAPI_GENERIC_FAILURE
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_delete_async_work(_env: Env, _work: *mut c_void) -> NapiStatus {
    NAPI_GENERIC_FAILURE
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_queue_async_work(_env: Env, _work: *mut c_void) -> NapiStatus {
    NAPI_GENERIC_FAILURE
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_cancel_async_work(_env: Env, _work: *mut c_void) -> NapiStatus {
    NAPI_GENERIC_FAILURE
}

// ------------------------------------------------------------------ threadsafe functions
//
// oam has no libuv event loop, so true cross-thread dispatch is not supported.
// napi-rs 2.x creates a "GC ThreadsafeFunction" during module init to schedule
// finalizer calls back onto the JS thread.  If napi_create_threadsafe_function
// returns failure, napi-rs panics immediately.  We return NAPI_OK with a dummy
// opaque handle so the creation succeeds; napi_call_threadsafe_function is a
// no-op (finalizers are silently dropped -- acceptable for a sync-only host).

struct TsfnStub {
    context: *mut c_void,
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_threadsafe_function(
    _env: Env,
    _func: NapiValue,
    _async_resource: NapiValue,
    _async_resource_name: NapiValue,
    _max_queue_size: usize,
    _initial_thread_count: usize,
    _thread_finalize_data: *mut c_void,
    _thread_finalize_cb: *mut c_void,
    context: *mut c_void,
    _call_js_cb: *mut c_void,
    result: *mut *mut c_void,
) -> NapiStatus {
    if result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let stub = Box::new(TsfnStub { context });
    // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
    unsafe { *result = Box::into_raw(stub) as *mut c_void };
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_threadsafe_function_context(
    func: *mut c_void,
    result: *mut *mut c_void,
) -> NapiStatus {
    if func.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    // SAFETY: `func` is the opaque threadsafe-function handle returned by napi_create_threadsafe_function (a Box::into_raw'd TsfnStub); checked non-null above.
    let stub = unsafe { &*(func as *const TsfnStub) };
    // SAFETY: `result` is a caller-provided out-parameter pointer, null-checked before this write.
    unsafe { *result = stub.context };
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_call_threadsafe_function(
    _func: *mut c_void,
    _data: *mut c_void,
    _is_blocking: u32,
) -> NapiStatus {
    // No event loop -- silently drop. Finalizers will not run.
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_acquire_threadsafe_function(_func: *mut c_void) -> NapiStatus {
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_release_threadsafe_function(
    _func: *mut c_void,
    _mode: u32,
) -> NapiStatus {
    // Leaks the TsfnStub -- acceptable for a sync host where addons live
    // until the runtime shuts down.
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_unref_threadsafe_function(
    _env: Env,
    _func: *mut c_void,
) -> NapiStatus {
    NAPI_OK
}

/// # Safety
///
/// A C-ABI N-API entry point called by loaded `.node` addons. `env` must be
/// the live `napi_env` this engine handed the addon, with a native entry
/// (trampoline or `load_addon`) on the stack; every `napi_value` argument
/// must be a handle valid in that scope; every out-pointer must be null or
/// valid for writes of its pointee type. Each dereference below is guarded
/// individually.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_ref_threadsafe_function(_env: Env, _func: *mut c_void) -> NapiStatus {
    NAPI_OK
}

// =========================================================== addon loading

type RegisterFn = unsafe extern "C" fn(Env, NapiValue) -> NapiValue;

/// Load a .node addon and run its napi_register_module_v1. Returns the
/// module's exports value, or None with a pending JS exception.
///
/// The `AddonRegistry` is retrieved from the isolate slot; both the
/// `NapiEnv` allocation and the `libloading::Library` handle are filed in
/// it, and what becomes of them when the owning `JsRuntime` drops is the
/// registry's teardown order (see [`AddonRegistry`]).
pub(crate) fn load_addon<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    path: &std::path::Path,
) -> Option<v8::Local<'s, v8::Value>> {
    // Several entry points in this file keep a `&mut NapiEnv` live across V8
    // calls on the strength of "this runs no JavaScript" (see the note in
    // `napi_create_function`). The part of that this runtime owns is the
    // explicit microtask policy `JsRuntime::new` sets: under the auto policy
    // V8 runs a microtask checkpoint whenever a hook-arming API call leaves
    // call depth zero.
    debug_assert_eq!(
        scope.get_microtasks_policy(),
        v8::MicrotasksPolicy::Explicit,
        "N-API addons need MicrotasksPolicy::Explicit: napi.rs holds env borrows across V8 calls on the grounds that no microtask checkpoint can run under them"
    );
    // Refused before anything is opened: see `AddonRegistry::closing`.
    if scope
        .get_slot::<AddonRegistry>()
        .is_some_and(|registry| registry.closing)
    {
        throw(
            scope,
            &format!(
                "cannot load addon {}: its runtime is shutting down",
                path.display()
            ),
        );
        return None;
    }
    let trace = std::env::var_os("OAM_NAPI_TRACE").is_some();
    if trace {
        eprintln!("[napi] dlopen start {}", path.display());
    }
    // SAFETY: loads the addon shared object at `path`; running an arbitrary `.node`'s initializers is inherently unsafe FFI, bounded by the AddonRegistry that keeps the library mapped for at least the runtime's life.
    let library = match unsafe { libloading::Library::new(path) } {
        Ok(library) => library,
        Err(e) => {
            throw(scope, &format!("cannot load addon {}: {e}", path.display()));
            return None;
        }
    };
    if trace {
        eprintln!("[napi] dlopen ok; looking up napi_register_module_v1");
    }
    let register: libloading::Symbol<RegisterFn> =
        // SAFETY: resolves `napi_register_module_v1` in the just-loaded `library`; the returned Symbol borrows `library`, which stays live until the fn pointer is copied out on the next lines.
        match unsafe { library.get(b"napi_register_module_v1") } {
            Ok(symbol) => symbol,
            Err(e) => {
                throw(
                    scope,
                    &format!(
                        "{} is not an N-API addon (napi_register_module_v1 missing): {e}",
                        path.display()
                    ),
                );
                return None;
            }
        };
    let register: RegisterFn = *register;

    // Hand the library and a fresh NapiEnv to the registry BEFORE running
    // register(). register() can create napi functions -- whose code pointers
    // live in this DLL -- and attach them to persistent JS objects
    // (globalThis, exports, or even the error it throws), and it can save the
    // env. Filing both here rather than only on the success path keeps the
    // pending-exception return below from dropping `library` and unmapping
    // code the surviving env still points into; from here on the registry's
    // teardown order governs both (see `AddonRegistry`).
    //
    // The raw pointer is used during registration (the Box heap address is
    // stable even as the registry's Vec reallocates).
    // Push FIRST, then derive the pointer from the element in place.
    //
    // Deriving from the Box and then MOVING it into the registry invalidates
    // the pointer: moving a Box is a typed move, so the tag `env` carries is
    // popped and every later use of it is UB. Confirmed mechanically --
    // `oam_aliasing_model::held_derive_then_move_box_is_ub` models the old
    // order and miri rejects it ("trying to retag ... for Unique permission,
    // but that tag does not exist in the borrow stack"). This is the same
    // file-then-derive idiom `create_function` uses for its `FnData`.
    // (napi_create_reference no longer hands out a pointer at all: a
    // `napi_ref` is an index+generation handle, see `encode_ref_handle`.)
    let env: Env = {
        let registry = scope
            .get_slot_mut::<AddonRegistry>()
            .expect("AddonRegistry slot installed");
        registry.addons.push(Addon {
            env: Some(NapiEnv::new()),
            library: Some(library),
            anchor: register as usize,
        });
        let addon = registry.addons.last_mut().expect("just pushed");
        &raw mut **addon.env.as_mut().expect("just filed")
    };

    let exports = v8::Object::new(scope);
    let exports_value: v8::Local<v8::Value> = exports.into();

    // SAFETY: `env` is this addon's freshly allocated NapiEnv; stashes the live PinScope pointer so nested napi calls during register() can recover it.
    unsafe {
        (*env).scope = scope as *mut v8::PinScope<'_, '_> as *mut c_void;
    }
    if trace {
        eprintln!("[napi] symbol found; calling register()");
    }
    // SAFETY: calls the addon's `napi_register_module_v1` (a C-ABI fn pointer from the loaded `.node`) with this env and the exports object; the addon then runs arbitrary native code -- the inherent FFI trust boundary.
    let result = unsafe { register(env, from_local(exports_value)) };
    if trace {
        eprintln!("[napi] register() returned");
    }
    // SAFETY: `env` is this addon's NapiEnv; clears the stashed scope pointer now that register() has returned and the borrow is ending.
    unsafe {
        (*env).scope = std::ptr::null_mut();
    }

    // SAFETY: `env` is a valid `*mut NapiEnv` (established above); reads/writes its `pending` deferred-exception slot.
    if let Some(pending) = unsafe { (*env).pending.take() } {
        let exception = v8::Local::new(scope, &pending);
        scope.throw_exception(exception);
        return None;
    }

    // SAFETY: `result` is a napi_value handle from this env; to_local reinterprets it as a repr-compatible `v8::Local` (size-asserted at module top) and returns None when null.
    match unsafe { to_local(result) } {
        Some(returned) => Some(returned),
        None => Some(exports_value),
    }
}

fn throw(scope: &mut v8::PinScope<'_, '_>, message: &str) {
    let message = v8::String::new(scope, message)
        .unwrap_or_else(|| v8::String::new(scope, "napi load error").unwrap());
    let exception = v8::Exception::error(scope, message);
    scope.throw_exception(exception);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::JsRuntime;

    // A buffer too short for the string gets whole characters only: the cut
    // backs up to the previous boundary instead of splitting a sequence.
    #[test]
    fn utf8_prefix_never_splits_a_character() {
        // "h" + U+00E9 (2 bytes) + "llo" = 6 bytes.
        let text = "h\u{e9}llo";
        assert_eq!(utf8_prefix_len(text, 0), 0);
        assert_eq!(utf8_prefix_len(text, 1), 1);
        assert_eq!(utf8_prefix_len(text, 2), 1);
        assert_eq!(utf8_prefix_len(text, 3), 3);
        assert_eq!(utf8_prefix_len(text, 6), 6);
        assert_eq!(utf8_prefix_len(text, 100), 6);
        // U+1F600 is 4 bytes: nothing fits until all four do.
        let wide = "\u{1F600}";
        assert_eq!(utf8_prefix_len(wide, 1), 0);
        assert_eq!(utf8_prefix_len(wide, 3), 0);
        assert_eq!(utf8_prefix_len(wide, 4), 4);
        assert_eq!(utf8_prefix_len("", 8), 0);
    }

    // Both branches under test are pure table bookkeeping, but reaching them
    // through the real C entry points is what makes the STATUS and the
    // last-error reason part of the assertion. `env` and `value` always come
    // from `JsRuntime::with_napi_env`, which installs a live `PinScope` on the
    // env for the closure's duration -- exactly the state a native entry
    // guarantees -- so the ABI contract is identical at every call site and is
    // discharged once, in the helpers below, rather than N times inline.

    /// `napi_create_reference` with refcount 1: its status, and the handle it
    /// wrote (null when it refused, since it writes nothing on failure).
    fn create_ref(env: Env, value: NapiValue) -> (NapiStatus, NapiRefHandle) {
        let mut handle: NapiRefHandle = std::ptr::null_mut();
        // SAFETY: `env` and `value` are the live env and napi_value handed to
        // the `with_napi_env` closure, which keeps a native scope installed for
        // its whole body; `handle` is a live local backing the out-pointer.
        let status = unsafe { napi_create_reference(env, value, 1, &mut handle) };
        (status, handle)
    }

    fn delete_ref(env: Env, handle: NapiRefHandle) -> NapiStatus {
        // SAFETY: env as in `create_ref`. `handle` is decoded rather than
        // dereferenced by the callee, so a stale or retired one is a miss.
        unsafe { napi_delete_reference(env, handle) }
    }

    fn ref_value(env: Env, handle: NapiRefHandle) -> NapiStatus {
        let mut value: NapiValue = std::ptr::null_mut();
        // SAFETY: env/handle as above; `value` is a live local out-pointer.
        unsafe { napi_get_reference_value(env, handle, &mut value) }
    }

    /// What `napi_get_last_error_info` reports for this env right now.
    fn last_error(env: Env) -> (NapiStatus, String) {
        let mut info: *const NapiExtendedErrorInfo = std::ptr::null();
        // SAFETY: env as above; `info` is a live local out-pointer. What comes
        // back borrows the env's own `last_error` slot, which outlives this
        // call, and its `error_message` is one of this module's `&'static
        // CStr` literals -- so both reads are in-bounds and NUL-terminated.
        unsafe {
            assert_eq!(napi_get_last_error_info(env, &mut info), NAPI_OK);
            let info = info.as_ref().expect("last error info must be written");
            let message = std::ffi::CStr::from_ptr(info.error_message)
                .to_string_lossy()
                .into_owned();
            (info.error_code, message)
        }
    }

    /// `(slots, free-list length, env tag)` -- the table bookkeeping nothing on
    /// the ABI exposes, which is exactly why both branches below were unproven.
    fn env_state(env: Env) -> (usize, usize, usize) {
        // SAFETY: `env` is the live `*mut NapiEnv` from `with_napi_env`; the
        // reborrow is SHARED, read-only, and ends with this statement.
        let env = unsafe { &*env };
        (env.refs.len(), env.ref_free.len(), env.ref_tag)
    }

    /// A full reference table must REFUSE, not mint a handle whose index is
    /// truncated into someone else's slot.
    ///
    /// Unreachable at the shipped limit -- it wants 16.7M live references on
    /// one env -- so `REF_MAX_SLOTS` shrinks under `cfg(test)`; only the
    /// threshold moves, not the encoding. The whole contract is asserted:
    /// `napi_generic_failure`, nothing pushed, no handle written, a reason
    /// recorded for `napi_get_last_error_info`, and every already-live handle
    /// still resolvable afterwards.
    #[test]
    fn create_reference_refuses_a_full_table() {
        let mut rt = JsRuntime::new();
        rt.with_napi_env(|env, value| {
            NAPI_REF_PUSH_COUNT.with(|c| c.set(0));

            let live: Vec<NapiRefHandle> = (0..REF_MAX_SLOTS)
                .map(|i| {
                    let (status, handle) = create_ref(env, value);
                    assert_eq!(status, NAPI_OK, "slot {i} is inside the table");
                    handle
                })
                .collect();
            assert_eq!(env_state(env).0, REF_MAX_SLOTS, "the table must be full");

            let (status, handle) = create_ref(env, value);
            assert_eq!(
                status, NAPI_GENERIC_FAILURE,
                "a create past REF_MAX_SLOTS must be refused"
            );
            assert!(handle.is_null(), "a refused create must write no handle");
            assert_eq!(
                NAPI_REF_PUSH_COUNT.with(|c| c.get()),
                REF_MAX_SLOTS,
                "a refused create must not push an entry"
            );
            assert_eq!(
                env_state(env).0,
                REF_MAX_SLOTS,
                "a refused create must not grow the table"
            );

            // Checked BEFORE the liveness sweep below: a successful entry point
            // clears the slot, so any OK call would erase the reason.
            let (code, message) = last_error(env);
            assert_eq!(code, NAPI_GENERIC_FAILURE, "the reason must be recorded");
            assert!(
                message.contains("reference table is full"),
                "last_error must say WHY, got: {message}"
            );

            for (i, handle) in live.iter().enumerate() {
                assert_eq!(
                    ref_value(env, *handle),
                    NAPI_OK,
                    "reference {i} must survive a refused create"
                );
            }
        });
    }

    /// A slot that exhausts its generations is RETIRED, not recycled.
    ///
    /// Also unreachable as shipped -- 16.7M create/delete cycles on one slot --
    /// so `REF_MAX_GENERATION` shrinks under `cfg(test)`. The observable
    /// invariant is the bookkeeping, not the arithmetic: the spent slot never
    /// returns to the free list, so the next create takes a FRESH slot rather
    /// than wrapping the counter back onto a generation an addon still holds.
    #[test]
    fn generation_exhaustion_retires_the_slot() {
        let mut rt = JsRuntime::new();
        rt.with_napi_env(|env, value| {
            let tag = env_state(env).2;

            // Cycle ONE slot until its generations are spent. The free list
            // hands slot 0 back every time, one generation further on.
            let mut spent: NapiRefHandle = std::ptr::null_mut();
            for generation in 1..=REF_MAX_GENERATION {
                let (status, handle) = create_ref(env, value);
                assert_eq!(status, NAPI_OK, "generation {generation} is spendable");
                assert_eq!(
                    decode_ref_handle(tag, handle),
                    Some((0, generation)),
                    "a recycled slot keeps its index and advances its generation"
                );
                assert_eq!(delete_ref(env, handle), NAPI_OK);
                spent = handle;
            }

            let (slots, free, _) = env_state(env);
            assert_eq!(
                (slots, free),
                (1, 0),
                "a slot at REF_MAX_GENERATION must be retired, not freed for reuse"
            );

            let (status, fresh) = create_ref(env, value);
            assert_eq!(status, NAPI_OK);
            assert_eq!(
                decode_ref_handle(tag, fresh),
                Some((1, 1)),
                "the next create must take a fresh slot, never the retired one"
            );
            assert_eq!(env_state(env).0, 2, "the table grew instead of recycling");

            // And the retired slot keeps refusing its own last handle. This is
            // the `entry.take()?` in `free_ref` doing it, NOT the generation
            // compare: a retired slot keeps the generation `spent` carries, so
            // that compare PASSES and the emptied entry is the only rejection
            // left -- which is the claim the comment there makes.
            assert_eq!(
                ref_value(env, spent),
                NAPI_INVALID_ARG,
                "a retired slot must not resolve"
            );
            assert_eq!(
                delete_ref(env, spent),
                NAPI_INVALID_ARG,
                "a retired slot must refuse a second delete"
            );
        });
    }

    /// A release ([`release_env`]) retires every reference it drops, one
    /// slot at a time, instead of resetting the table. Teardown runs addon
    /// code after an env's release -- the cleanup hooks registered before
    /// it, another release round -- and a reference minted there must not
    /// come out under the index and generation of one minted before, which
    /// a reset table hands out again: the old handle would then read, and
    /// delete, the new reference.
    #[test]
    fn a_release_retires_the_references_it_drops() {
        let mut rt = JsRuntime::new();
        rt.with_napi_env(|env, value| {
            let tag = env_state(env).2;
            let (status, before) = create_ref(env, value);
            assert_eq!(status, NAPI_OK);
            assert_eq!(decode_ref_handle(tag, before), Some((0, 1)));

            // SAFETY: `env` is the live env of `with_napi_env`, whose isolate
            // is alive; no reference into it is held across the call, and
            // nothing here registered a finalizer.
            unsafe { release_env(env) };
            assert_eq!(ref_value(env, before), NAPI_INVALID_ARG, "released");
            let (slots, free, _) = env_state(env);
            assert_eq!(
                (slots, free),
                (1, 1),
                "the slot is kept, retired onto the free list"
            );

            let (status, after) = create_ref(env, value);
            assert_eq!(status, NAPI_OK);
            assert_eq!(
                decode_ref_handle(tag, after),
                Some((0, 2)),
                "the slot is reused one generation on"
            );
            assert_ne!(after, before, "a handle from before the release came back");
            assert_eq!(
                ref_value(env, before),
                NAPI_INVALID_ARG,
                "a handle from before the release resolved to a newer reference"
            );
            assert_eq!(
                delete_ref(env, before),
                NAPI_INVALID_ARG,
                "a handle from before the release deleted a newer reference"
            );
            assert_eq!(
                ref_value(env, after),
                NAPI_OK,
                "the newer reference is intact"
            );
            assert_eq!(delete_ref(env, after), NAPI_OK);
        });
    }

    // ------------------------------------------------------------------
    // The tests below drive the entry points that call into JS. Same
    // contract as above: `env` and every `napi_value` come from
    // `with_napi_env` (or from a call made inside its closure), which keeps
    // a native scope installed for the closure's whole body.

    /// Evaluate `source` through `napi_run_script`.
    fn eval(env: Env, source: &std::ffi::CStr) -> NapiValue {
        let mut text: NapiValue = std::ptr::null_mut();
        let mut value: NapiValue = std::ptr::null_mut();
        // SAFETY: `env` is the live env of the `with_napi_env` closure;
        // `source` is a NUL-terminated literal, read with NAPI_AUTO_LENGTH;
        // `text` and `value` are live locals backing the out-pointers.
        unsafe {
            assert_eq!(
                napi_create_string_utf8(env, source.as_ptr(), usize::MAX, &mut text),
                NAPI_OK
            );
            assert_eq!(napi_run_script(env, text, &mut value), NAPI_OK);
        }
        value
    }

    fn strict_eq(env: Env, a: NapiValue, b: NapiValue) -> bool {
        let mut same = false;
        // SAFETY: env and both handles as in `eval`; `same` is a live local.
        unsafe { assert_eq!(napi_strict_equals(env, a, b, &mut same), NAPI_OK) };
        same
    }

    fn is_pending(env: Env) -> bool {
        let mut pending = false;
        // SAFETY: env as in `eval`; `pending` is a live local out-pointer.
        unsafe { assert_eq!(napi_is_exception_pending(env, &mut pending), NAPI_OK) };
        pending
    }

    /// Clear the pending exception and hand it back (a null handle is never
    /// written: with nothing pending the entry point writes `undefined`).
    fn take_exception(env: Env) -> NapiValue {
        let mut caught: NapiValue = std::ptr::null_mut();
        // SAFETY: env as in `eval`; `caught` is a live local out-pointer.
        unsafe { assert_eq!(napi_get_and_clear_last_exception(env, &mut caught), NAPI_OK) };
        caught
    }

    fn named(env: Env, object: NapiValue, name: &std::ffi::CStr) -> NapiValue {
        let mut value: NapiValue = std::ptr::null_mut();
        // SAFETY: env and `object` as in `eval`; `name` is a NUL-terminated
        // literal; `value` is a live local out-pointer.
        unsafe {
            assert_eq!(
                napi_get_named_property(env, object, name.as_ptr(), &mut value),
                NAPI_OK
            )
        };
        value
    }

    /// A null `argv` with a non-zero count, or a null element, used to be
    /// read (the first) or silently skipped (the second), which called the
    /// function with the later arguments shifted one place left.
    #[test]
    fn a_missing_argument_is_refused_not_skipped() {
        let mut rt = JsRuntime::new();
        rt.with_napi_env(|env, value| {
            let second = eval(env, c"(function (a, b) { return b; })");
            let marker = eval(env, c"'marker'");
            let mut returned: NapiValue = std::ptr::null_mut();

            let holed = [std::ptr::null_mut(), marker];
            // SAFETY: env and handles as in `eval`; `holed` is a live array
            // of exactly the 2 elements declared; `returned` is a live local.
            let status =
                unsafe { napi_call_function(env, value, second, 2, holed.as_ptr(), &mut returned) };
            assert_eq!(status, NAPI_INVALID_ARG, "a null element must be refused");

            // SAFETY: as above; the null `argv` is the input under test and
            // must be rejected before it is read.
            let status = unsafe {
                napi_call_function(env, value, second, 1, std::ptr::null(), &mut returned)
            };
            assert_eq!(status, NAPI_INVALID_ARG, "a null argv must be refused");

            // SAFETY: as above, for the constructor path.
            let status =
                unsafe { napi_new_instance(env, second, 1, std::ptr::null(), &mut returned) };
            assert_eq!(status, NAPI_INVALID_ARG, "new_instance checks argv too");

            // Control: the same call with every argument present reaches the
            // function with its arguments in place.
            let full = [value, marker];
            // SAFETY: as the first call, with no null element.
            let status =
                unsafe { napi_call_function(env, value, second, 2, full.as_ptr(), &mut returned) };
            assert_eq!(status, NAPI_OK);
            assert!(
                strict_eq(env, returned, marker),
                "argument 1 must arrive as b"
            );
        });
    }

    /// An exception a JS callee raises has to be visible through the env's
    /// own pending-exception API, and clearable through it.
    #[test]
    fn a_js_throw_becomes_the_pending_exception() {
        let mut rt = JsRuntime::new();
        rt.with_napi_env(|env, value| {
            let thrower = eval(env, c"(function () { throw new RangeError('from js'); })");
            let mut returned: NapiValue = std::ptr::null_mut();
            // SAFETY: env and handles as in `eval`; zero arguments, so the
            // null `argv` is never read; `returned` is a live local.
            let status = unsafe {
                napi_call_function(env, value, thrower, 0, std::ptr::null(), &mut returned)
            };
            assert_eq!(status, NAPI_PENDING_EXCEPTION);
            assert!(is_pending(env), "the throw must be reported as pending");

            // While one is pending, a call that could run JS is refused
            // rather than run on top of it.
            let mut text: NapiValue = std::ptr::null_mut();
            // SAFETY: env as in `eval`; the source is a NUL-terminated
            // literal; `text` and `returned` are live locals.
            unsafe {
                assert_eq!(
                    napi_create_string_utf8(env, c"1".as_ptr(), usize::MAX, &mut text),
                    NAPI_OK
                );
                assert_eq!(
                    napi_run_script(env, text, &mut returned),
                    NAPI_PENDING_EXCEPTION
                );
            }

            let caught = take_exception(env);
            assert!(!is_pending(env), "get_and_clear must clear it");
            let mut is_error = false;
            // SAFETY: env and `caught` as in `eval`; `is_error` is a live local.
            unsafe { assert_eq!(napi_is_error(env, caught, &mut is_error), NAPI_OK) };
            assert!(is_error, "the cleared value is the thrown error");
            assert!(strict_eq(
                env,
                named(env, caught, c"message"),
                eval(env, c"'from js'")
            ));

            // A property write whose setter throws used to answer napi_ok.
            let trap = eval(env, c"({ set x(v) { throw new Error('setter'); } })");
            // SAFETY: env and handles as in `eval`; the name is a
            // NUL-terminated literal.
            let status = unsafe { napi_set_named_property(env, trap, c"x".as_ptr(), value) };
            assert_eq!(status, NAPI_PENDING_EXCEPTION);
            assert!(is_pending(env));
            take_exception(env);

            // A SyntaxError from napi_run_script is pending the same way.
            // SAFETY: as the create_string / run_script pair above.
            unsafe {
                assert_eq!(
                    napi_create_string_utf8(env, c"(".as_ptr(), usize::MAX, &mut text),
                    NAPI_OK
                );
                assert_eq!(
                    napi_run_script(env, text, &mut returned),
                    NAPI_PENDING_EXCEPTION
                );
            }
            assert!(is_pending(env));
            take_exception(env);
        });
    }

    /// napi_create_error and its two siblings ignored `code`; the throw_*
    /// variants always set it.
    #[test]
    fn created_errors_carry_their_code() {
        let mut rt = JsRuntime::new();
        rt.with_napi_env(|env, value| {
            let code = eval(env, c"'E_SAMPLE'");
            let message = eval(env, c"'sample'");
            type Create =
                unsafe extern "C" fn(Env, NapiValue, NapiValue, *mut NapiValue) -> NapiStatus;
            let creators: [Create; 3] = [
                napi_create_error,
                napi_create_type_error,
                napi_create_range_error,
            ];
            for create in creators {
                let mut error: NapiValue = std::ptr::null_mut();
                // SAFETY: env and handles as in `eval`; `error` is a live local.
                unsafe { assert_eq!(create(env, code, message, &mut error), NAPI_OK) };
                assert!(strict_eq(env, named(env, error, c"code"), code));

                // A null code adds no property.
                // SAFETY: as above; a null `code` is the documented "none".
                unsafe {
                    assert_eq!(
                        create(env, std::ptr::null_mut(), message, &mut error),
                        NAPI_OK
                    )
                };
                assert!(strict_eq(
                    env,
                    named(env, error, c"code"),
                    eval(env, c"undefined")
                ));

                // A code that is not a string is refused.
                // SAFETY: as above; `value` is an object, the input under test.
                let status = unsafe { create(env, value, message, &mut error) };
                assert_eq!(status, NAPI_STRING_EXPECTED);
            }
        });
    }

    /// The exception left pending has the given `code`, and is cleared.
    fn pending_code_is(env: Env, code: &std::ffi::CStr) -> bool {
        if !is_pending(env) {
            return false;
        }
        let caught = take_exception(env);
        strict_eq(env, named(env, caught, c"code"), eval(env, code))
    }

    /// A view that is misaligned, or does not fit its ArrayBuffer, is a
    /// RangeError with Node's code instead of an unchecked V8 construction.
    #[test]
    fn views_are_checked_against_their_arraybuffer() {
        let mut rt = JsRuntime::new();
        rt.with_napi_env(|env, _value| {
            const INT16: u32 = 3;
            let mut buffer: NapiValue = std::ptr::null_mut();
            // SAFETY: env as in `eval`; a null `data` out-pointer is allowed;
            // `buffer` is a live local.
            unsafe {
                assert_eq!(
                    napi_create_arraybuffer(env, 8, std::ptr::null_mut(), &mut buffer),
                    NAPI_OK
                )
            };
            let mut view: NapiValue = std::ptr::null_mut();
            let mut typed = |count: usize, offset: usize| {
                // SAFETY: env and `buffer` as in `eval`; `view` is a live local.
                unsafe { napi_create_typedarray(env, INT16, count, buffer, offset, &mut view) }
            };

            assert_eq!(
                typed(1, 1),
                NAPI_GENERIC_FAILURE,
                "odd offset for 2-byte elements"
            );
            assert!(pending_code_is(
                env,
                c"'ERR_NAPI_INVALID_TYPEDARRAY_ALIGNMENT'"
            ));

            assert_eq!(typed(5, 0), NAPI_GENERIC_FAILURE, "10 bytes do not fit 8");
            assert!(pending_code_is(
                env,
                c"'ERR_NAPI_INVALID_TYPEDARRAY_LENGTH'"
            ));

            assert_eq!(
                typed(usize::MAX, 0),
                NAPI_GENERIC_FAILURE,
                "overflowing length"
            );
            assert!(pending_code_is(
                env,
                c"'ERR_NAPI_INVALID_TYPEDARRAY_LENGTH'"
            ));

            assert_eq!(typed(3, 2), NAPI_OK, "2 + 3 * 2 == 8 fits exactly");
            assert!(!is_pending(env));

            let data_view = |size: usize, offset: usize| {
                let mut out_view: NapiValue = std::ptr::null_mut();
                // SAFETY: env and `buffer` as in `eval`; `out_view` is a live local.
                unsafe { napi_create_dataview(env, size, buffer, offset, &mut out_view) }
            };
            assert_eq!(data_view(8, 1), NAPI_PENDING_EXCEPTION, "1 + 8 > 8");
            assert!(pending_code_is(env, c"'ERR_NAPI_INVALID_DATAVIEW_ARGS'"));
            assert_eq!(data_view(7, 1), NAPI_OK, "1 + 7 == 8 fits exactly");
        });
    }

    /// An explicit length is honoured exactly: the text need not be
    /// NUL-terminated, and nothing past the length is read.
    #[test]
    fn sized_text_reads_exactly_the_given_length() {
        let unterminated = *b"abcdef";
        let ptr = unterminated.as_ptr().cast::<c_char>();
        // SAFETY: `ptr` is valid for the 6 bytes of `unterminated`; every
        // length passed is within them.
        unsafe {
            assert_eq!(sized_text(ptr, 3).as_deref(), Some("abc"));
            assert_eq!(sized_text(ptr, 6).as_deref(), Some("abcdef"));
            assert_eq!(sized_text(ptr, 0).as_deref(), Some(""));
        }
        // SAFETY: a NUL-terminated literal, read with NAPI_AUTO_LENGTH.
        let auto = unsafe { sized_text(c"terminated".as_ptr(), usize::MAX) };
        assert_eq!(auto.as_deref(), Some("terminated"));
        // SAFETY: null is the documented "no string" input.
        assert_eq!(unsafe { sized_text(std::ptr::null(), 4) }, None);
    }

    #[test]
    fn napi_string_length_accepts_auto_and_up_to_i32_max() {
        assert!(is_napi_string_length(usize::MAX));
        assert!(is_napi_string_length(0));
        assert!(is_napi_string_length(i32::MAX as usize));
        assert!(!is_napi_string_length(i32::MAX as usize + 1));
        assert!(!is_napi_string_length(usize::MAX - 1));
    }

    #[test]
    fn napi_string_args_allow_a_null_text_only_with_length_zero() {
        let text = c"ab".as_ptr().cast::<c_void>();
        let mut slot: NapiValue = std::ptr::null_mut();
        let out: *mut NapiValue = &mut slot;
        let null = std::ptr::null::<c_void>();
        assert!(is_napi_string_args(null, 0, out));
        assert!(!is_napi_string_args(null, 2, out));
        assert!(!is_napi_string_args(null, usize::MAX, out));
        assert!(is_napi_string_args(text, 2, out));
        assert!(is_napi_string_args(text, usize::MAX, out));
        assert!(!is_napi_string_args(text, i32::MAX as usize + 1, out));
        assert!(!is_napi_string_args(text, 2, std::ptr::null_mut()));
    }

    #[test]
    fn napi_text_units_reads_the_given_length_or_to_the_terminator() {
        let wide: [u16; 4] = [0x61, 0x62, 0x63, 0];
        // SAFETY: each pointer is null with length 0, or a live buffer at least as long as the length read (0-terminated for usize::MAX).
        unsafe {
            assert!(napi_text_units(std::ptr::null::<u8>(), 0).is_empty());
            assert!(napi_text_units(c"ab".as_ptr().cast::<u8>(), 0).is_empty());
            assert_eq!(napi_text_units(c"abTAIL".as_ptr().cast::<u8>(), 2), b"ab");
            assert_eq!(
                napi_text_units(c"auto".as_ptr().cast::<u8>(), usize::MAX),
                b"auto"
            );
            assert_eq!(napi_text_units(wide.as_ptr(), 2), &wide[..2]);
            assert_eq!(napi_text_units(wide.as_ptr(), usize::MAX), &wide[..3]);
        }
    }

    // Sum of the `data` addresses the finalizer below was called with.
    // Not a doc comment -- rustdoc can't document items produced by a macro.
    thread_local! {
        static FINALIZED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    extern "C" fn record_finalize(_env: Env, data: *mut c_void, _hint: *mut c_void) {
        FINALIZED.with(|c| c.set(c.get() + data.addr()));
    }

    /// A wrap is found again by its object, hands back a usable weak
    /// reference, and owes its finalizer exactly once: at release for a wrap
    /// still in place, never for one taken off with `napi_remove_wrap`.
    #[test]
    fn wrap_roundtrip_reference_and_finalizer() {
        FINALIZED.with(|c| c.set(0));
        let kept_native = std::ptr::without_provenance_mut::<c_void>(0x10);
        let removed_native = std::ptr::without_provenance_mut::<c_void>(0x200);
        let mut rt = JsRuntime::new();
        rt.with_napi_env(|env, value| {
            let finalize: NapiFinalize = record_finalize;
            let kept = eval(env, c"({})");
            let removed = eval(env, c"({})");

            let mut weak: NapiRefHandle = std::ptr::null_mut();
            // SAFETY: env and handles as in `eval`. The native pointers are
            // identity tokens the host stores and hands back, never
            // dereferenced by it or by `record_finalize`; `weak` is a live
            // local out-pointer, and the second call passes the documented
            // null for "no reference wanted".
            unsafe {
                assert_eq!(
                    napi_wrap(
                        env,
                        kept,
                        kept_native,
                        Some(finalize),
                        std::ptr::null_mut(),
                        &mut weak,
                    ),
                    NAPI_OK
                );
                assert_eq!(
                    napi_wrap(
                        env,
                        removed,
                        removed_native,
                        Some(finalize),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    ),
                    NAPI_OK
                );
            }
            assert!(!weak.is_null(), "napi_wrap must hand back a reference");
            let mut referenced: NapiValue = std::ptr::null_mut();
            // SAFETY: env as in `eval`; `weak` was just minted by this env;
            // `referenced` is a live local.
            unsafe {
                assert_eq!(
                    napi_get_reference_value(env, weak, &mut referenced),
                    NAPI_OK
                )
            };
            assert!(
                strict_eq(env, referenced, kept),
                "the reference must resolve to the wrapped object"
            );

            let unwrap = |object: NapiValue| {
                let mut native: *mut c_void = std::ptr::null_mut();
                // SAFETY: env and `object` as in `eval`; `native` is a live local.
                let status = unsafe { napi_unwrap(env, object, &mut native) };
                (status, native)
            };
            assert_eq!(unwrap(kept), (NAPI_OK, kept_native));
            assert_eq!(unwrap(removed), (NAPI_OK, removed_native));
            assert_eq!(unwrap(value).0, NAPI_INVALID_ARG, "never wrapped");

            let mut native: *mut c_void = std::ptr::null_mut();
            // SAFETY: env and `removed` as in `eval`; `native` is a live local.
            let status = unsafe { napi_remove_wrap(env, removed, &mut native) };
            assert_eq!((status, native), (NAPI_OK, removed_native));
            assert_eq!(unwrap(removed).0, NAPI_INVALID_ARG, "the wrap is gone");
            assert_eq!(unwrap(kept), (NAPI_OK, kept_native), "its neighbour is not");

            assert_eq!(FINALIZED.with(|c| c.get()), 0, "nothing was collected yet");
        });
        // `with_napi_env` releases the env the way runtime teardown does.
        assert_eq!(
            FINALIZED.with(|c| c.get()),
            0x10,
            "the wrap still in place is finalized once; the removed one never"
        );
    }

    /// The collector's half of a finalizer: when V8 collects a tracked
    /// object it records the entry's id and nothing else, and the entry is
    /// handed over -- with the native pointer it was registered with -- the
    /// next time the env asks. An object that is still reachable is neither
    /// reported nor handed over. Uses only the env's safe surface: the V8
    /// finalizer holds the id queue, never the env, so there is no pointer
    /// here to get wrong.
    #[test]
    fn a_collected_object_is_reported_by_id_and_taken_once() {
        let lost_native = std::ptr::without_provenance_mut::<c_void>(0x40);
        let kept_native = std::ptr::without_provenance_mut::<c_void>(0x80);
        let mut rt = JsRuntime::new();
        v8::scope_with_context!(let scope, &mut rt.isolate, &rt.context);
        let mut env = NapiEnv::new();

        let kept = v8::Object::new(scope);
        let kept_id = env.track_weak(scope, kept.into(), kept_native, None, std::ptr::null_mut());
        let lost_id = {
            // Its only handle dies with this scope.
            v8::scope!(let inner, scope);
            let lost = v8::Object::new(inner);
            env.track_weak(inner, lost.into(), lost_native, None, std::ptr::null_mut())
        };
        assert!(
            env.take_collected().is_empty(),
            "nothing has been collected yet"
        );

        scope.low_memory_notification();
        assert_eq!(
            *env.collected.borrow(),
            [lost_id],
            "the collector reports the unreachable object's id, and only that"
        );
        let owed = env.take_collected();
        assert_eq!(owed.len(), 1);
        assert_eq!(owed[0].native, lost_native);
        assert!(env.collected.borrow().is_empty(), "the report is consumed");
        assert!(env.take_collected().is_empty(), "and handed over only once");
        assert!(!env.tracked.contains_key(&lost_id));
        assert_eq!(
            env.tracked.get(&kept_id).map(|entry| entry.native),
            Some(kept_native),
            "a reachable object stays tracked"
        );
        assert!(kept.is_object(), "and its handle is still usable");

        // Released while the isolate is alive, as `release_env` does.
        drop(owed);
        env.tracked.clear();
    }

    // ------------------------------------------------------------------
    // The tests below collect objects an addon made, so the object's only
    // handle has to die first. A handle made in the `with_napi_env` closure
    // itself lives as long as the closure does; one made inside a native
    // callback dies when the callback returns, because V8 opens a handle
    // scope around every API callback. So everything that creates or reads
    // a handle to the object under test runs through `native_call`.

    /// Run `body` as the callback of a function built by
    /// `napi_create_function` and called through `napi_call_function` --
    /// the path every addon call takes, `napi_trampoline` included -- and
    /// return the call's status. `body` must not panic (that would unwind
    /// out of an `extern "C"` frame): it records, and the test asserts
    /// afterwards.
    fn native_call(env: Env, recv: NapiValue, mut body: &mut dyn FnMut(Env)) -> NapiStatus {
        extern "C" fn run_body(env: Env, info: *mut CbInfo) -> NapiValue {
            let mut data: *mut c_void = std::ptr::null_mut();
            // SAFETY: `env` and `info` are what the trampoline handed this
            // callback; the three null out-pointers are the documented "not
            // wanted"; `data` is a live local. What comes back is the
            // pointer `native_call` registered below: its own `body` slot,
            // live and unaliased for the whole of the one call made through
            // this function.
            unsafe {
                napi_get_cb_info(
                    env,
                    info,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut data,
                );
                if let Some(body) = data.cast::<&mut dyn FnMut(Env)>().as_mut() {
                    body(env);
                }
            }
            std::ptr::null_mut()
        }
        let data = (&raw mut body).cast::<c_void>();
        let mut function: NapiValue = std::ptr::null_mut();
        let mut returned: NapiValue = std::ptr::null_mut();
        // SAFETY: `env` and `recv` are the live env and napi_value of the
        // `with_napi_env` closure; a null name is allowed; `data` points at
        // `body`, which outlives the single call below (the function is
        // never called again); zero arguments, so the null `argv` is never
        // read; `function` and `returned` are live locals.
        unsafe {
            assert_eq!(
                napi_create_function(
                    env,
                    std::ptr::null(),
                    0,
                    Some(run_body),
                    data,
                    &mut function
                ),
                NAPI_OK
            );
            napi_call_function(env, recv, function, 0, std::ptr::null(), &mut returned)
        }
    }

    /// A full collection, from the `with_napi_env` closure body.
    fn collect_garbage(env: Env) {
        // SAFETY: `env` is the live env of the `with_napi_env` closure, with
        // its scope installed; this is called between native calls, when no
        // other reference to that scope is in use.
        let scope = unsafe { env_scope_mut(env) }.expect("a native scope is installed");
        scope.low_memory_notification();
    }

    // `(env address, data address, hint address)` of every call the
    // finalizer below received, in order.
    thread_local! {
        static COLLECTED_LOG: std::cell::RefCell<Vec<(usize, usize, usize)>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }

    /// The hint that makes `record_collected` leave an exception pending.
    const THROWING_HINT: usize = 0xBAD;

    extern "C" fn record_collected(env: Env, data: *mut c_void, hint: *mut c_void) {
        COLLECTED_LOG.with(|log| {
            log.borrow_mut()
                .push((env.addr(), data.addr(), hint.addr()))
        });
        if hint.addr() == THROWING_HINT {
            // SAFETY: `env` is the env the host called this finalizer with,
            // scope installed; a null code is allowed and the message is a
            // NUL-terminated literal.
            unsafe { napi_throw_error(env, std::ptr::null(), c"from a finalizer".as_ptr()) };
        }
    }

    fn collected_log() -> Vec<(usize, usize, usize)> {
        COLLECTED_LOG.with(|log| log.borrow().clone())
    }

    /// Wrap a fresh object, inside a native call so that no handle to it
    /// outlives the call, with `record_collected` as its finalizer. `result`
    /// is `napi_wrap`'s optional reference out-pointer.
    fn wrap_unreachable_object(
        env: Env,
        recv: NapiValue,
        native: usize,
        hint: usize,
        result: *mut NapiRefHandle,
    ) {
        let finalize: NapiFinalize = record_collected;
        let mut status = NAPI_GENERIC_FAILURE;
        let call = native_call(env, recv, &mut |env| {
            let mut object: NapiValue = std::ptr::null_mut();
            // SAFETY: `env` is the callback's env; `object` is a live local,
            // written by the first call and a valid handle for the second;
            // the native pointer and hint are identity tokens nothing
            // dereferences; `result` is null or the caller's live local.
            status = unsafe {
                napi_create_object(env, &mut object);
                napi_wrap(
                    env,
                    object,
                    std::ptr::without_provenance_mut(native),
                    Some(finalize),
                    std::ptr::without_provenance_mut(hint),
                    result,
                )
            };
        });
        assert_eq!((call, status), (NAPI_OK, NAPI_OK), "the wrap must succeed");
    }

    /// The whole collect -> finalize pipeline: the collector only reports,
    /// and the addon's finalizer runs at the NEXT native entry -- before the
    /// callback, once, with the env, native pointer and hint it was
    /// registered with. A finalizer that leaves an exception pending makes
    /// that entry throw it instead of running the callback.
    #[test]
    fn a_collected_wrap_is_finalized_at_the_next_native_entry() {
        COLLECTED_LOG.with(|log| log.borrow_mut().clear());
        let mut rt = JsRuntime::new();
        let env_addr = rt.with_napi_env(|env, value| {
            wrap_unreachable_object(env, value, 0x40, 0x7, std::ptr::null_mut());
            collect_garbage(env);
            assert!(
                collected_log().is_empty(),
                "the collector must not run addon code"
            );

            // The next native entry owes the finalizer, and pays it before
            // the callback runs.
            let mut seen_by_callback = usize::MAX;
            let status = native_call(env, value, &mut |_env| {
                seen_by_callback = collected_log().len();
            });
            assert_eq!(status, NAPI_OK);
            assert_eq!(
                seen_by_callback, 1,
                "the finalizer must have run before the callback"
            );
            assert_eq!(
                collected_log(),
                [(env.addr(), 0x40, 0x7)],
                "called with the env, native pointer and hint it was registered with"
            );

            // And only once, however many collections and entries follow.
            collect_garbage(env);
            assert_eq!(native_call(env, value, &mut |_env| {}), NAPI_OK);
            assert_eq!(collected_log().len(), 1, "a finalizer runs exactly once");

            // A finalizer that throws: the entry that ran it reports the
            // exception and does not run its callback.
            wrap_unreachable_object(env, value, 0x80, THROWING_HINT, std::ptr::null_mut());
            collect_garbage(env);
            let mut callback_ran = false;
            let status = native_call(env, value, &mut |_env| callback_ran = true);
            assert_eq!(status, NAPI_PENDING_EXCEPTION);
            assert!(
                !callback_ran,
                "the callback must be skipped when a finalizer left an exception pending"
            );
            assert_eq!(
                collected_log(),
                [(env.addr(), 0x40, 0x7), (env.addr(), 0x80, THROWING_HINT)]
            );
            assert!(is_pending(env));
            let caught = take_exception(env);
            assert!(strict_eq(
                env,
                named(env, caught, c"message"),
                eval(env, c"'from a finalizer'")
            ));

            // The exception is spent with that entry: the next one runs.
            let mut callback_ran = false;
            let status = native_call(env, value, &mut |_env| callback_ran = true);
            assert_eq!(status, NAPI_OK);
            assert!(callback_ran);
            env.addr()
        });
        // Releasing the env finalizes what is still tracked -- nothing here.
        assert_eq!(
            collected_log(),
            [(env_addr, 0x40, 0x7), (env_addr, 0x80, THROWING_HINT)],
            "an entry finalized after collection is not finalized again at release"
        );
    }

    type NapiCallback = unsafe extern "C" fn(Env, *mut CbInfo) -> NapiValue;

    /// `FnData` records freed so far on this thread.
    fn fn_records_freed() -> usize {
        NAPI_FN_DATA_FREED_COUNT.with(|c| c.get())
    }

    /// `FnData` records filed and not freed yet.
    fn fn_records_live() -> usize {
        NAPI_FN_DATA_FILED_COUNT.with(|c| c.get()) - fn_records_freed()
    }

    /// Build a function around `cb`, inside a native call so that no handle
    /// to it outlives the call. With `hold`, a count-1 reference to it is
    /// returned and is then the only thing keeping it alive; without, it is
    /// unreachable as soon as this returns (and the handle is null).
    fn function_made_in_a_call(
        env: Env,
        recv: NapiValue,
        cb: NapiCallback,
        hold: bool,
    ) -> NapiRefHandle {
        let mut handle: NapiRefHandle = std::ptr::null_mut();
        let mut status = NAPI_GENERIC_FAILURE;
        let call = native_call(env, recv, &mut |env| {
            let mut function: NapiValue = std::ptr::null_mut();
            // SAFETY: `env` is the callback's env; a null name is allowed;
            // `cb` is a callback of this test binary that ignores its data
            // pointer; `function` is a live local, written by the first call
            // and a valid handle for the second; `handle` is a live local
            // out-pointer.
            status = unsafe {
                match napi_create_function(
                    env,
                    std::ptr::null(),
                    0,
                    Some(cb),
                    std::ptr::null_mut(),
                    &mut function,
                ) {
                    NAPI_OK if hold => napi_create_reference(env, function, 1, &mut handle),
                    created => created,
                }
            };
        });
        assert_eq!(
            (call, status),
            (NAPI_OK, NAPI_OK),
            "the create must succeed"
        );
        handle
    }

    /// Call the function `handle` refers to, from inside a native call so
    /// the handle the call is made through dies with it. The status of the
    /// call itself.
    fn call_through(env: Env, recv: NapiValue, handle: NapiRefHandle) -> NapiStatus {
        let mut status = NAPI_GENERIC_FAILURE;
        let call = native_call(env, recv, &mut |env| {
            let mut function: NapiValue = std::ptr::null_mut();
            let mut returned: NapiValue = std::ptr::null_mut();
            // SAFETY: `env` is the callback's env; `handle` is decoded, not
            // dereferenced; `function` is a live local the first call writes
            // (a null value makes the second refuse); `recv` is a live
            // napi_value of the `with_napi_env` closure; zero arguments, so
            // the null `argv` is never read; `returned` is a live local.
            status = unsafe {
                napi_get_reference_value(env, handle, &mut function);
                napi_call_function(env, recv, function, 0, std::ptr::null(), &mut returned)
            };
        });
        assert_eq!(call, NAPI_OK);
        status
    }

    thread_local! {
        // Calls `count_call` has received.
        static COUNTED_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        // For `orphan_itself`: the one reference to the function it is the
        // callback of, a receiver for the calls it makes, and what it saw.
        static OWN_REFERENCE: std::cell::Cell<NapiRefHandle> =
            const { std::cell::Cell::new(std::ptr::null_mut()) };
        static OWN_RECEIVER: std::cell::Cell<NapiValue> =
            const { std::cell::Cell::new(std::ptr::null_mut()) };
        static FREED_IN_OWN_CALL: std::cell::Cell<Option<(NapiStatus, usize)>> =
            const { std::cell::Cell::new(None) };
    }

    extern "C" fn count_call(_env: Env, _info: *mut CbInfo) -> NapiValue {
        COUNTED_CALLS.with(|c| c.set(c.get() + 1));
        std::ptr::null_mut()
    }

    /// A callback that, while it is running, deletes the only reference to
    /// its own function and then does everything that could release a
    /// record: two full collections, each followed by a nested native entry
    /// (which is where records are released). It reports the status of the
    /// delete and how many records had been freed by the end of all that.
    extern "C" fn orphan_itself(env: Env, _info: *mut CbInfo) -> NapiValue {
        let deleted = delete_ref(env, OWN_REFERENCE.with(|c| c.get()));
        let recv = OWN_RECEIVER.with(|c| c.get());
        for _ in 0..2 {
            collect_garbage(env);
            native_call(env, recv, &mut |_env| {});
        }
        FREED_IN_OWN_CALL.with(|c| c.set(Some((deleted, fn_records_freed()))));
        std::ptr::null_mut()
    }

    /// The record behind an addon function (`FnData`) is released after the
    /// function is collected -- at the next native entry, not in the
    /// collector, and once -- and at no other time: not while the function
    /// is reachable, not under a call made through it, and whatever is left
    /// goes with the env, once.
    #[test]
    fn a_collected_function_releases_its_record_at_the_next_native_entry() {
        NAPI_FN_DATA_FILED_COUNT.with(|c| c.set(0));
        NAPI_FN_DATA_FREED_COUNT.with(|c| c.set(0));
        let mut rt = JsRuntime::new();
        let left_at_release = rt.with_napi_env(|env, value| {
            // Every `native_call` made from this closure leaves its own
            // function reachable until the closure ends (its handle belongs
            // to the closure's scope), so those records stay: the counts
            // below move only for the functions made INSIDE a call.

            // (1) An unreachable function: nothing is freed by creating it
            // or by collecting it ...
            function_made_in_a_call(env, value, count_call, false);
            assert_eq!(fn_records_freed(), 0);
            let live = fn_records_live();
            collect_garbage(env);
            assert_eq!(
                (fn_records_freed(), fn_records_live()),
                (0, live),
                "the collector must not free a record"
            );
            // ... its record goes at the next native entry, before the
            // callback runs (the entry files one record of its own) ...
            let mut seen_by_callback = usize::MAX;
            let status = native_call(env, value, &mut |_env| {
                seen_by_callback = fn_records_freed();
            });
            assert_eq!(status, NAPI_OK);
            assert_eq!(
                seen_by_callback, 1,
                "the record must be freed at the native entry, before the callback"
            );
            assert_eq!((fn_records_freed(), fn_records_live()), (1, live));
            // ... and only once, however many collections and entries follow.
            collect_garbage(env);
            assert_eq!(native_call(env, value, &mut |_env| {}), NAPI_OK);
            assert_eq!(fn_records_freed(), 1, "a record is freed exactly once");

            // (2) A function that is still reachable keeps its record across
            // collections and native entries, and can still be called.
            let held = function_made_in_a_call(env, value, count_call, true);
            for calls in 1..=2 {
                collect_garbage(env);
                assert_eq!(native_call(env, value, &mut |_env| {}), NAPI_OK);
                assert_eq!(
                    fn_records_freed(),
                    1,
                    "a reachable function must keep its record"
                );
                assert_eq!(call_through(env, value, held), NAPI_OK);
                assert_eq!(COUNTED_CALLS.with(|c| c.get()), calls);
            }

            // (3) A function that drops the last reference to itself from
            // inside its own call, then collects and re-enters native code
            // twice: no record is freed while its callback runs -- not its
            // own, and not those of the two functions the nested entries
            // made, which that callback's scope still holds.
            let own = function_made_in_a_call(env, value, orphan_itself, true);
            OWN_REFERENCE.with(|c| c.set(own));
            OWN_RECEIVER.with(|c| c.set(value));
            assert_eq!(call_through(env, value, own), NAPI_OK);
            assert_eq!(
                FREED_IN_OWN_CALL.with(|c| c.get()),
                Some((NAPI_OK, 1)),
                "no record may be freed under the running callback"
            );
            assert_eq!(
                ref_value(env, own),
                NAPI_INVALID_ARG,
                "the callback deleted the only reference to its function"
            );
            // The call has returned: all three are garbage now, and are
            // released the usual way -- not by the collection ...
            let live = fn_records_live();
            collect_garbage(env);
            assert_eq!((fn_records_freed(), fn_records_live()), (1, live));
            // ... but at the next native entry (which files one record).
            assert_eq!(native_call(env, value, &mut |_env| {}), NAPI_OK);
            assert_eq!(
                (fn_records_freed(), fn_records_live()),
                (4, live + 1 - 3),
                "the function that orphaned itself and the two it made are released, once each"
            );
            collect_garbage(env);
            assert_eq!(native_call(env, value, &mut |_env| {}), NAPI_OK);
            assert_eq!(fn_records_freed(), 4);

            // The function held since (2) is untouched by all of that.
            assert_eq!(call_through(env, value, held), NAPI_OK);
            assert_eq!(COUNTED_CALLS.with(|c| c.get()), 3);
            assert_eq!(fn_records_freed(), 4);
            fn_records_live()
        });
        // (4) `with_napi_env` releases and drops the env the way runtime
        // teardown does: every record still filed goes with it, once.
        assert!(left_at_release > 1, "the held function and the callers");
        assert_eq!(
            (fn_records_freed(), fn_records_live()),
            (4 + left_at_release, 0),
            "the env frees what is left, exactly once"
        );
    }

    /// Create a reference with `initial` count to a fresh object, inside a
    /// native call so that the reference is the only thing that can keep the
    /// object alive.
    fn reference_to_unreachable_object(env: Env, recv: NapiValue, initial: u32) -> NapiRefHandle {
        let mut handle: NapiRefHandle = std::ptr::null_mut();
        let mut status = NAPI_GENERIC_FAILURE;
        let call = native_call(env, recv, &mut |env| {
            let mut object: NapiValue = std::ptr::null_mut();
            // SAFETY: `env` is the callback's env; `object` is a live local,
            // written by the first call and a valid handle for the second;
            // `handle` is a live local out-pointer.
            status = unsafe {
                napi_create_object(env, &mut object);
                napi_create_reference(env, object, initial, &mut handle)
            };
        });
        assert_eq!(
            (call, status),
            (NAPI_OK, NAPI_OK),
            "the create must succeed"
        );
        handle
    }

    type RefCountOp = unsafe extern "C" fn(Env, NapiRefHandle, *mut u32) -> NapiStatus;

    /// `napi_reference_ref` / `napi_reference_unref` from inside a native
    /// call, where an addon makes them: the count the call reported.
    fn recount(env: Env, recv: NapiValue, handle: NapiRefHandle, op: RefCountOp) -> u32 {
        let mut count = u32::MAX;
        let mut status = NAPI_GENERIC_FAILURE;
        let call = native_call(env, recv, &mut |env| {
            // SAFETY: `env` is the callback's env; `handle` is decoded, not
            // dereferenced; `count` is a live local out-pointer.
            status = unsafe { op(env, handle, &mut count) };
        });
        assert_eq!((call, status), (NAPI_OK, NAPI_OK));
        count
    }

    /// Whether `handle` still resolves to an object, read from inside a
    /// native call so the read itself leaves no handle behind. A collected
    /// weak reference must answer `napi_ok` with a null value, which is
    /// `false` here; any other status fails the test.
    fn resolves(env: Env, recv: NapiValue, handle: NapiRefHandle) -> bool {
        let mut value: NapiValue = std::ptr::without_provenance_mut(1);
        let mut status = NAPI_GENERIC_FAILURE;
        let call = native_call(env, recv, &mut |env| {
            // SAFETY: `env` is the callback's env; `handle` is decoded, not
            // dereferenced; `value` is a live local out-pointer, and what it
            // receives is only compared with null, never used as a handle.
            status = unsafe { napi_get_reference_value(env, handle, &mut value) };
        });
        assert_eq!((call, status), (NAPI_OK, NAPI_OK));
        !value.is_null()
    }

    /// A reference keeps its object alive exactly while its count is
    /// positive. 1 -> 0 lets the object go; 0 -> 1 holds it again; 0 -> 1 on
    /// an object already collected stays empty; and a collected reference
    /// reads back as a null value with `napi_ok`.
    #[test]
    fn a_reference_holds_its_object_only_at_a_positive_count() {
        let mut rt = JsRuntime::new();
        rt.with_napi_env(|env, value| {
            // Control: a count-0 reference never held its object.
            let weak = reference_to_unreachable_object(env, value, 0);
            assert!(resolves(env, value, weak), "alive until a collection");
            collect_garbage(env);
            assert!(
                !resolves(env, value, weak),
                "a weak reference holds nothing"
            );

            // 1 -> 0 stops holding.
            let strong = reference_to_unreachable_object(env, value, 1);
            collect_garbage(env);
            assert!(
                resolves(env, value, strong),
                "a count of 1 holds the object"
            );
            assert_eq!(recount(env, value, strong, napi_reference_unref), 0);
            assert!(resolves(env, value, strong), "alive until a collection");
            collect_garbage(env);
            assert!(
                !resolves(env, value, strong),
                "at a count of 0 the reference must let the object go"
            );
            // 0 -> 1 on a collected object: the count moves, nothing comes back.
            assert_eq!(recount(env, value, strong, napi_reference_ref), 1);
            collect_garbage(env);
            assert!(
                !resolves(env, value, strong),
                "a collected object stays gone"
            );
            assert_eq!(recount(env, value, strong, napi_reference_unref), 0);
            // Saturating: a count of 0 does not wrap.
            assert_eq!(recount(env, value, strong, napi_reference_unref), 0);

            // 0 -> 1 holds again, 1 -> 2 -> 1 changes nothing, 1 -> 0 lets go.
            let held = reference_to_unreachable_object(env, value, 0);
            assert_eq!(recount(env, value, held, napi_reference_ref), 1);
            collect_garbage(env);
            assert!(
                resolves(env, value, held),
                "a reference taken from 0 to 1 must hold its object"
            );
            assert_eq!(recount(env, value, held, napi_reference_ref), 2);
            assert_eq!(recount(env, value, held, napi_reference_unref), 1);
            collect_garbage(env);
            assert!(resolves(env, value, held), "still held at a count of 1");
            assert_eq!(recount(env, value, held, napi_reference_unref), 0);
            collect_garbage(env);
            assert!(!resolves(env, value, held), "and released again at 0");
        });
    }

    /// The count-0 reference `napi_wrap` hands back is a real reference:
    /// taking it to 1 keeps the wrapped object alive (so its finalizer is
    /// not owed), and back at 0 the object goes and the finalizer runs.
    #[test]
    fn the_reference_a_wrap_hands_back_can_hold_the_wrapped_object() {
        COLLECTED_LOG.with(|log| log.borrow_mut().clear());
        let mut rt = JsRuntime::new();
        rt.with_napi_env(|env, value| {
            let mut handle: NapiRefHandle = std::ptr::null_mut();
            wrap_unreachable_object(env, value, 0x100, 0, &mut handle);
            assert!(!handle.is_null(), "napi_wrap must hand back a reference");

            assert_eq!(recount(env, value, handle, napi_reference_ref), 1);
            collect_garbage(env);
            assert!(resolves(env, value, handle), "held through the reference");
            assert!(collected_log().is_empty(), "so no finalizer is owed");

            assert_eq!(recount(env, value, handle, napi_reference_unref), 0);
            collect_garbage(env);
            assert!(!resolves(env, value, handle), "released at a count of 0");
            assert_eq!(
                collected_log(),
                [(env.addr(), 0x100, 0)],
                "and the wrap's finalizer ran at the native entry after the collection"
            );
        });
        assert_eq!(collected_log().len(), 1, "not again at release");
    }

    /// An external ArrayBuffer aliases the caller's memory instead of
    /// copying it, and a null pointer is only acceptable with a zero length.
    #[test]
    fn external_arraybuffer_aliases_the_callers_memory() {
        // Declared before the runtime so it outlives every ArrayBuffer made
        // over it: no finalizer is registered, which is the caller's promise
        // that the memory stays valid.
        let mut bytes = [1u8, 2, 3, 4];
        let base = bytes.as_mut_ptr().cast::<c_void>();
        let mut rt = JsRuntime::new();
        rt.with_napi_env(|env, _value| {
            let info = |buffer: NapiValue| {
                let mut data: *mut c_void = std::ptr::null_mut();
                let mut len = usize::MAX;
                // SAFETY: env and `buffer` as in `eval`; both out-pointers
                // are live locals.
                unsafe {
                    assert_eq!(
                        napi_get_arraybuffer_info(env, buffer, &mut data, &mut len),
                        NAPI_OK
                    )
                };
                (data, len)
            };

            let mut buffer: NapiValue = std::ptr::null_mut();
            // SAFETY: env as in `eval`; `base` is valid for the 4 bytes
            // declared and outlives the runtime; `buffer` is a live local.
            let status = unsafe {
                napi_create_external_arraybuffer(
                    env,
                    base,
                    4,
                    None,
                    std::ptr::null_mut(),
                    &mut buffer,
                )
            };
            assert_eq!(status, NAPI_OK);
            assert_eq!(info(buffer), (base, 4), "the buffer must alias, not copy");

            // SAFETY: as above; a null pointer with a zero length is read
            // by nobody.
            let status = unsafe {
                napi_create_external_arraybuffer(
                    env,
                    std::ptr::null_mut(),
                    0,
                    None,
                    std::ptr::null_mut(),
                    &mut buffer,
                )
            };
            assert_eq!(
                status, NAPI_OK,
                "null data with no length is an empty buffer"
            );
            assert_eq!(info(buffer).1, 0);

            // SAFETY: as above; the null pointer with a length is the input
            // under test and must be refused before it is used.
            let status = unsafe {
                napi_create_external_arraybuffer(
                    env,
                    std::ptr::null_mut(),
                    4,
                    None,
                    std::ptr::null_mut(),
                    &mut buffer,
                )
            };
            assert_eq!(status, NAPI_INVALID_ARG);
        });
    }

    // What `record_new_target` saw on its last call: whether
    // napi_get_new_target reported a constructor.
    // Not a doc comment -- rustdoc can't document items produced by a macro.
    thread_local! {
        static SAW_NEW_TARGET: std::cell::Cell<Option<bool>> =
            const { std::cell::Cell::new(None) };
    }

    extern "C" fn record_new_target(env: Env, info: *mut CbInfo) -> NapiValue {
        let mut target: NapiValue = std::ptr::null_mut();
        // SAFETY: called only by the trampoline, with its live env and the
        // CbInfo on its stack; `target` is a live local out-pointer.
        let status = unsafe { napi_get_new_target(env, info, &mut target) };
        SAW_NEW_TARGET.with(|c| c.set(Some(status == NAPI_OK && !target.is_null())));
        std::ptr::null_mut()
    }

    extern "C" fn seven(env: Env, _info: *mut CbInfo) -> NapiValue {
        let mut value: NapiValue = std::ptr::null_mut();
        // SAFETY: called only by the trampoline, with its live env; `value`
        // is a live local out-pointer.
        unsafe { napi_create_int32(env, 7, &mut value) };
        value
    }

    /// `new.target` reaches a native constructor, and is null for a plain
    /// call -- the test a class constructor uses to refuse being called
    /// without `new`.
    #[test]
    fn new_target_distinguishes_construct_from_call() {
        let mut rt = JsRuntime::new();
        rt.with_napi_env(|env, value| {
            let mut function: NapiValue = std::ptr::null_mut();
            let mut returned: NapiValue = std::ptr::null_mut();
            // SAFETY: env as in `eval`; the name is a NUL-terminated literal
            // read with NAPI_AUTO_LENGTH; the callback has the exact
            // `napi_callback` signature; zero arguments, so the null `argv`
            // is never read; the out-pointers are live locals.
            unsafe {
                assert_eq!(
                    napi_create_function(
                        env,
                        c"Probe".as_ptr(),
                        usize::MAX,
                        Some(record_new_target),
                        std::ptr::null_mut(),
                        &mut function,
                    ),
                    NAPI_OK
                );
                SAW_NEW_TARGET.with(|c| c.set(None));
                assert_eq!(
                    napi_new_instance(env, function, 0, std::ptr::null(), &mut returned),
                    NAPI_OK
                );
                assert_eq!(SAW_NEW_TARGET.with(|c| c.get()), Some(true), "construct");

                assert_eq!(
                    napi_call_function(env, value, function, 0, std::ptr::null(), &mut returned,),
                    NAPI_OK
                );
                assert_eq!(SAW_NEW_TARGET.with(|c| c.get()), Some(false), "plain call");
            }
        });
    }

    /// A descriptor with a getter defines a working accessor; these were
    /// skipped while the call still answered napi_ok.
    #[test]
    fn a_getter_descriptor_defines_an_accessor() {
        let mut rt = JsRuntime::new();
        rt.with_napi_env(|env, value| {
            let getter: unsafe extern "C" fn(Env, *mut CbInfo) -> NapiValue = seven;
            let descriptor = NapiPropertyDescriptor {
                utf8name: c"seven".as_ptr(),
                name: std::ptr::null_mut(),
                method: None,
                getter: Some(getter),
                setter: None,
                value: std::ptr::null_mut(),
                attributes: NAPI_ATTR_ENUMERABLE,
                data: std::ptr::null_mut(),
            };
            // SAFETY: env and `value` (an object) as in `eval`; `descriptor`
            // is a live local, read as the 1 element declared; its name is a
            // NUL-terminated literal and its getter a `napi_callback`.
            unsafe { assert_eq!(napi_define_properties(env, value, 1, &descriptor), NAPI_OK) };
            assert!(strict_eq(env, named(env, value, c"seven"), eval(env, c"7")));
        });
    }
}
