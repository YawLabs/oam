//! Miri-checkable models of the raw-pointer disciplines in
//! `crates/oam_engine/src/napi.rs`.
//!
//! # Why this crate exists
//!
//! Every aliasing claim in this repo has been *argued*, never machine-checked:
//! the 2026-08-21 unsafe audit, the two aliasing-UB fixes in #84, and the
//! use-after-free fix in #86 all rest on reading code and reasoning about
//! Stacked/Tree Borrows. That is a weak footing for the surface real MCP
//! sidecar addons load, and it has already been demonstrated to be weak: of
//! three candidate N-API wrapper designs reviewed in 2026-08-26, one
//! (`Frame<'a>`, which stored `&mut PinScope` and held `&mut NapiEnv` across a
//! re-entrant call) would have INTRODUCED fresh UB while passing every gate
//! this repo has.
//!
//! Miri is the tool that checks this class mechanically. It cannot be pointed
//! at `oam_engine` directly, for two independent reasons:
//!
//! 1. **Miri cannot execute foreign functions.** `oam_engine` links V8; every
//!    interesting path calls into it.
//! 2. **`cargo-miri` cannot build a large dependency graph on Windows.** Cargo
//!    switches to an argfile on long command lines, and `cargo-miri` rejects
//!    that outright: `cargo uses an argfile to invoke rustc, which is not
//!    supported by cargo-miri`. This blocks the build before FFI is even
//!    reached.
//!
//! So this crate MODELS the disciplines instead: same pointer operations, same
//! order, stub types in place of V8's. It has zero dependencies, which is what
//! keeps blocker 2 away, and it must stay that way.
//!
//! # What a model does and does not prove
//!
//! Stated plainly, because the distinction is the whole value of this crate:
//!
//! - It DOES prove the *shape* is sound or unsound under Miri's borrow model.
//!   A double-`&mut` derived from one raw pointer is UB whether the pointee is
//!   a `v8::PinScope` or the `StubScope` below; Miri sees the same violation.
//! - It does NOT prove `napi.rs` performs that shape. That link is made by the
//!   `mirrors:` note on each model naming the function it was read from, and
//!   it is only as good as those notes. If `napi.rs` changes, a model can
//!   silently go stale -- so treat a note as a claim to re-check, not as a
//!   guarantee. The notes name functions, not line numbers: line numbers
//!   went stale within weeks every time they were recorded here.
//!
//! The notes were last re-read against `napi.rs` in 2026-11, when the
//! reference table had become index+generation handles (no pointer into the
//! table ever leaves `napi.rs`) and the models of the reference half were
//! rewritten to match. The older boxed-entry design survives only in
//! `held_deref_deleted_handle_is_use_after_free`, as the bug it was.
//!
//! Later the same month the weak-tracking half was added (`StubEnv::tracked`
//! / `collected`, [`StubCollector`]). Its V8 finalizers do not hold the env:
//! each holds a clone of an `Rc` id queue, and the env takes the collected
//! entries out by id at the next native entry. That is the one thing that
//! lets `napi_wrap` and its neighbours keep a `&mut NapiEnv` across V8 calls
//! that allocate, and `collector_reports_by_id_under_a_live_env_borrow` is
//! its machine check; `held_collector_writing_through_the_env_is_ub` is the
//! design it replaced.
//!
//! The exclusive scope came back the same month, deliberately:
//! `napi.rs::env_scope_mut` hands out `&mut PinScope` for the operations V8
//! only offers through `&mut` (a `TryCatch`, a `v8::Weak`), under the rule
//! that an entry point takes ONE scope reference and no second one while it
//! lives. [`scope_exclusive`] is that helper.
//! `one_exclusive_scope_with_a_live_ref_entry` and
//! `nested_entry_takes_its_own_exclusive_scope` are the machine checks of
//! the rule as the code uses it, and `held_two_exclusive_scopes_is_ub` is
//! what breaking it looks like -- the #84 bug, by the same helper.
//!
//! Function records followed, still in 2026-11: `napi_create_function` used
//! to keep every `FnData` for the env's life. They are now keyed by a tracked
//! id (`StubEnv::fn_data`), the `External` that carries a record's address
//! is tracked weakly like a wrap, and the record is dropped when its id comes
//! out of the queue at a native entry. `napi_trampoline` copies the record's
//! fields out before it runs anything.
//! `function_record_is_released_by_id_and_never_under_a_call` is the machine
//! check of that lifetime rule, and `boxed_record_pointer_survives_table_growth`
//! of the pointer the `External` holds into the table.
//!
//! Teardown, the same month: an env now outlives its registry whenever the
//! addon library that was handed its address can still run, and the addon
//! may present that address again from a library destructor. The registry
//! therefore never moves an env's box once the pointer is out -- it strips
//! the env through a pointer derived in place, frees it in place, and
//! forgets its `Vec` to leave the rest allocated.
//! `tombstone_outlives_its_registry` is the machine check that the addon's
//! own, OLD pointer stays usable through all of that.
//!
//! # Running
//!
//! ```text
//! cargo +nightly miri test -p oam_aliasing_model
//! ```
//!
//! The `held_*` tests are the teeth: they model the shapes that were ACTUAL
//! bugs, are `#[ignore]`d because Miri is expected to reject them, and are run
//! deliberately with `-- --ignored` to confirm this harness still detects what
//! it claims to. A harness whose failing cases have gone quiet is worse than
//! no harness, because it reads as coverage.

#![forbid(unsafe_op_in_unsafe_fn)]

/// Stands in for `v8::PinScope`. The real type is
/// `PinnedRef<'s, HandleScope<'i>>` = `Pin<&'s mut HandleScope<'i>>`, i.e. the
/// object at the stashed address itself owns a `&mut`. The field here exists
/// so reads and writes are real accesses Miri can see; its type is irrelevant.
#[derive(Debug)]
pub struct StubScope {
    pub tick: u64,
}

/// Stands in for `NapiEnv`. Only the fields the models exercise: the stashed
/// scope pointer, the boxed callback payloads handed out by raw pointer, the
/// reference table addressed by handle, the weakly-tracked objects with the
/// queue their collection is reported on, and the last-error slot.
#[derive(Debug)]
pub struct StubEnv {
    /// Mirrors `NapiEnv::scope`: `*mut c_void` holding a `*mut PinScope`,
    /// non-null only while a native entry is on the stack.
    pub scope: *mut StubScope,
    /// Mirrors `NapiEnv::fn_data`: `HashMap<usize, Box<FnData>>`, keyed by
    /// the `tracked` id of the function's `External`. The `Box` is what the
    /// pointer handed to V8 (the `External` payload) relies on: the table
    /// moves its values when it grows.
    pub fn_data: std::collections::HashMap<usize, Box<StubFnData>>,
    /// Mirrors `NapiEnv::refs`: `Vec<NapiRefSlot>`, addressed by an
    /// index+generation handle. No `Box`: nothing holds a pointer INTO this
    /// table, so its elements are free to move when it grows.
    pub refs: Vec<StubSlot>,
    /// Mirrors `NapiEnv::ref_free`: indices of free slots.
    pub ref_free: Vec<usize>,
    /// Mirrors `NapiEnv::ref_tag`: folded into every handle this env mints.
    pub ref_tag: usize,
    /// Mirrors `NapiEnv::tracked`: the weakly-tracked objects (wraps and
    /// external buffers), keyed by the id minted from `next_tracked_id`.
    pub tracked: std::collections::BTreeMap<usize, StubWrapEntry>,
    /// Mirrors `NapiEnv::next_tracked_id`.
    pub next_tracked_id: usize,
    /// Mirrors `NapiEnv::collected`: ids of `tracked` entries whose object
    /// has been collected. The finalizers registered with the collector hold
    /// a clone of this `Rc` -- and nothing else of the env.
    pub collected: std::rc::Rc<std::cell::RefCell<Vec<usize>>>,
    /// Mirrors `NapiEnv::last_error`, which `fail` / `ok` write through a
    /// fresh `&mut *env`.
    pub last_error: u32,
}

/// Stands in for `NapiWrapEntry`: the native pointer an addon registered for
/// an object (a plain number here; it is never dereferenced by the host).
#[derive(Debug, PartialEq, Eq)]
pub struct StubWrapEntry {
    pub native: u64,
}

/// Stands in for the part of V8 that owns weak-handle finalizers: rusty_v8
/// keeps each `Box<dyn FnOnce(&mut Isolate)>` in the isolate's finalizer map
/// and runs it from the garbage collector, i.e. from INSIDE whatever V8 call
/// triggered the collection, while the caller's own borrows are still live.
#[derive(Default)]
pub struct StubCollector {
    finalizers: Vec<(usize, Box<dyn FnOnce()>)>,
}

impl StubCollector {
    /// Run (and consume) the finalizer registered for tracked object `id`,
    /// as a collection of that object would.
    pub fn collect(&mut self, id: usize) {
        if let Some(at) = self.finalizers.iter().position(|(owner, _)| *owner == id) {
            let (_, finalizer) = self.finalizers.remove(at);
            finalizer();
        }
    }
}

/// Stands in for `FnData`.
#[derive(Debug)]
pub struct StubFnData {
    pub data: u64,
}

/// Stands in for `NapiRefEntry`. `strong` stands in for which arm of
/// `NapiRefValue` the entry holds (`Strong(Global)` / `Weak(Weak)`): the
/// thing `retarget_ref` rewrites when the count crosses zero.
#[derive(Debug)]
pub struct StubEntry {
    pub value: u64,
    pub refcount: u32,
    pub strong: bool,
}

/// Stands in for `NapiRefSlot`: `entry` is `None` while the slot is free, and
/// `generation` is bumped on every free.
#[derive(Debug)]
pub struct StubSlot {
    pub entry: Option<StubEntry>,
    pub generation: usize,
}

/// Stands in for `NapiRefHandle`. The real handle packs the same three
/// numbers into one pointer-sized integer (`encode_ref_handle`); the packing
/// is bit arithmetic and not part of any aliasing claim, so the model keeps
/// them apart. What matters is carried over: a handle is DATA, never a
/// pointer, and resolving it never dereferences anything but the env.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StubRefHandle {
    tag: usize,
    index: usize,
    generation: usize,
}

impl StubEnv {
    pub fn new() -> Self {
        Self::with_tag(1)
    }

    /// An env whose handles carry `tag`; two envs with different tags refuse
    /// each other's handles.
    pub fn with_tag(tag: usize) -> Self {
        Self {
            scope: std::ptr::null_mut(),
            fn_data: std::collections::HashMap::new(),
            refs: Vec::new(),
            ref_free: Vec::new(),
            ref_tag: tag,
            tracked: std::collections::BTreeMap::new(),
            next_tracked_id: 1,
            collected: std::rc::Rc::default(),
            last_error: 0,
        }
    }

    /// mirrors: `NapiEnv::track_weak` -- mint an id, register a finalizer
    /// that captures the id and a clone of the `collected` queue (never the
    /// env), file the entry.
    pub fn track_weak(&mut self, collector: &mut StubCollector, native: u64) -> usize {
        let id = self.mint_tracked_id();
        self.track_weak_as(id, collector, native);
        id
    }

    /// mirrors: `NapiEnv::mint_tracked_id` -- a fresh id, never reused.
    pub fn mint_tracked_id(&mut self) -> usize {
        let id = self.next_tracked_id;
        self.next_tracked_id = id + 1;
        id
    }

    /// mirrors: `NapiEnv::track_weak_as` -- the registration half of
    /// `track_weak`, under an id minted beforehand.
    pub fn track_weak_as(&mut self, id: usize, collector: &mut StubCollector, native: u64) {
        let collected = std::rc::Rc::clone(&self.collected);
        collector
            .finalizers
            .push((id, Box::new(move || collected.borrow_mut().push(id))));
        self.tracked.insert(id, StubWrapEntry { native });
    }

    /// mirrors: `napi.rs::create_function`, the env half -- mint the id,
    /// file the boxed record under it, derive the pointer from the record IN
    /// PLACE (`&raw mut **fn_data.entry(id).or_insert_with(..)`), then track
    /// the `External` created around that pointer under the same id. Returns
    /// the id and the pointer the `External` carries.
    pub fn file_function(
        &mut self,
        collector: &mut StubCollector,
        data: u64,
    ) -> (usize, *mut StubFnData) {
        let id = self.mint_tracked_id();
        let record: *mut StubFnData = &raw mut **self
            .fn_data
            .entry(id)
            .or_insert_with(|| Box::new(StubFnData { data }));
        self.track_weak_as(id, collector, 0);
        (id, record)
    }

    /// mirrors: `NapiEnv::take_collected` -- drain the reported ids, drop
    /// the function record filed under each (if it names a function) and
    /// take their entries out of `tracked`.
    pub fn take_collected(&mut self) -> Vec<StubWrapEntry> {
        let ids = std::mem::take(&mut *self.collected.borrow_mut());
        ids.into_iter()
            .filter_map(|id| {
                self.fn_data.remove(&id);
                self.tracked.remove(&id)
            })
            .collect()
    }

    /// mirrors: `NapiEnv::alloc_ref` -- reuse a free slot or push a fresh one
    /// (generation 1), install the entry, mint the handle.
    pub fn alloc_ref(&mut self, entry: StubEntry) -> StubRefHandle {
        let index = match self.ref_free.pop() {
            Some(index) => index,
            None => {
                self.refs.push(StubSlot {
                    entry: None,
                    generation: 1,
                });
                self.refs.len() - 1
            }
        };
        let slot = &mut self.refs[index];
        slot.entry = Some(entry);
        StubRefHandle {
            tag: self.ref_tag,
            index,
            generation: slot.generation,
        }
    }

    /// mirrors: `NapiEnv::free_ref` -- take the entry out, bump the
    /// generation, put the index on the free list.
    pub fn free_ref(&mut self, handle: StubRefHandle) -> Option<StubEntry> {
        if handle.tag != self.ref_tag {
            return None;
        }
        let slot = self.refs.get_mut(handle.index)?;
        if slot.generation != handle.generation {
            return None;
        }
        let entry = slot.entry.take()?;
        slot.generation += 1;
        self.ref_free.push(handle.index);
        Some(entry)
    }

    /// mirrors: `NapiEnv::ref_slot_entry` -- a tag compare, a bounds check
    /// and a generation compare; no pointer but `self` is touched.
    fn ref_slot_entry(&self, handle: StubRefHandle) -> Option<&StubEntry> {
        if handle.tag != self.ref_tag {
            return None;
        }
        let slot = self.refs.get(handle.index)?;
        if slot.generation != handle.generation {
            return None;
        }
        slot.entry.as_ref()
    }

    /// mirrors: `NapiEnv::ref_slot_entry_mut`.
    fn ref_slot_entry_mut(&mut self, handle: StubRefHandle) -> Option<&mut StubEntry> {
        if handle.tag != self.ref_tag {
            return None;
        }
        let slot = self.refs.get_mut(handle.index)?;
        if slot.generation != handle.generation {
            return None;
        }
        slot.entry.as_mut()
    }
}

impl Default for StubEnv {
    fn default() -> Self {
        Self::new()
    }
}

// ===================================================================== scope

/// Recover the stashed scope as a SHARED reference.
///
/// mirrors: `napi.rs::env_scope` as it stands after #84 --
/// `unsafe fn env_scope<'a>(env: Env) -> Option<&'a v8::PinScope<'static, 'static>>`,
/// reading the field with `as_ref` and casting through `*const`.
///
/// # Safety
///
/// `env` must point to a live `StubEnv` whose `scope` is either null or points
/// to a live `StubScope`.
pub unsafe fn scope_shared<'a>(env: *mut StubEnv) -> Option<&'a StubScope> {
    // SAFETY: caller guarantees `env` is live or null; `as_ref` guards null,
    // and the read only takes a shared reborrow of the stashed pointer VALUE.
    unsafe {
        let env_ref = env.as_ref()?;
        (env_ref.scope as *const StubScope).as_ref()
    }
}

/// Recover the stashed scope as an EXCLUSIVE reference.
///
/// mirrors: `napi.rs::env_scope_mut` --
/// `unsafe fn env_scope_mut<'a>(env: Env) -> Option<&'a mut v8::PinScope<'static, 'static>>`,
/// reading the field through a SHARED reborrow of the env (`as_ref`) and
/// turning the stashed pointer back into the `&mut` it was stored from.
/// Shipping code: `catching`, `napi_wrap`, `napi_add_finalizer`,
/// `napi_reference_ref` / `_unref` and the other entry points that need a
/// `TryCatch` or a `v8::Weak` take their scope this way.
///
/// It is also the shape `env_scope` itself had before #84, when EVERY entry
/// point took its scope exclusively and two of them overlapped; that bug is
/// `held_two_exclusive_scopes_is_ub`, which calls this twice. What changed
/// is not the helper but the rule around it, stated in its contract below.
///
/// # Safety
///
/// As [`scope_shared`], and the result must be the only reference to that
/// scope object in use for as long as it lives: no second call to this, and
/// no use of a [`scope_shared`] result, until it is dead. A nested native
/// entry is not a second call in that sense -- it installs a DIFFERENT scope
/// object first (`nested_entry_takes_its_own_exclusive_scope`).
pub unsafe fn scope_exclusive<'a>(env: *mut StubEnv) -> Option<&'a mut StubScope> {
    // SAFETY: caller guarantees `env` is live or null; `as_ref` guards null,
    // and the read only takes a shared reborrow of the stashed pointer
    // VALUE. Exclusivity over the scope object is the caller's obligation.
    unsafe {
        let env_ref = env.as_ref()?;
        (env_ref.scope).as_mut()
    }
}

// ================================================================ references

/// Resolve a handle to its entry through a SHARED reborrow of the env.
///
/// mirrors: `napi.rs::ref_entry` -- `env.as_ref()?.ref_slot_entry(ref_)`.
/// The handle is decoded, never dereferenced, so the only pointer this
/// touches is `env`. (It replaced the #86 design, a `ptr::eq` scan over boxed
/// entries followed by a deref of the caller's pointer.)
///
/// # Safety
///
/// `env` must be null or point to a live `StubEnv` that no `&mut` is live
/// over. `handle` may be stale or foreign: rejecting those is what this
/// models.
pub unsafe fn ref_entry<'a>(env: *mut StubEnv, handle: StubRefHandle) -> Option<&'a StubEntry> {
    // SAFETY: caller guarantees `env` is live or null; `as_ref` guards null,
    // and the reborrow is shared.
    unsafe { env.as_ref()?.ref_slot_entry(handle) }
}

/// Resolve a handle to its entry through an EXCLUSIVE reborrow of the env.
///
/// mirrors: `napi.rs::ref_entry_mut` -- `env.as_mut()?.ref_slot_entry_mut(ref_)`,
/// used by `napi_reference_ref` / `napi_reference_unref`. The result must be
/// dead before anything else derives a reference from `env`. Those callers
/// keep it live through the count update, the copy of the count and
/// `retarget_ref(scope, entry)`, and only then call `ok(env)`, which takes
/// its own `&mut *env`. The exclusive scope they hold alongside it was
/// derived BEFORE the lookup and points at the scope object, not into the
/// env, so it is not "another reference derived from `env`" in this sense.
///
/// # Safety
///
/// As [`ref_entry`], and nothing else may derive a reference from `env`
/// while the result is live.
pub unsafe fn ref_entry_mut<'a>(
    env: *mut StubEnv,
    handle: StubRefHandle,
) -> Option<&'a mut StubEntry> {
    // SAFETY: caller guarantees `env` is live or null and unaliased; `as_mut`
    // guards null.
    unsafe { env.as_mut()?.ref_slot_entry_mut(handle) }
}

/// Re-hold an entry's value the way its current count calls for: strong at
/// a positive count, weak at zero.
///
/// mirrors: `napi.rs::retarget_ref(scope: &mut PinScope, entry: &mut
/// NapiRefEntry)` -- safe code that is handed BOTH exclusive references at
/// once, reads the entry, creates the new persistent handle through the
/// scope and writes it back into the entry. The write to `scope.tick`
/// stands in for V8 using the scope to create that handle.
pub fn retarget_ref(scope: &mut StubScope, entry: &mut StubEntry) {
    let want_strong = entry.refcount != 0;
    if want_strong == entry.strong {
        return;
    }
    scope.tick += 1;
    entry.strong = want_strong;
}

/// Clear the last-error slot and return the OK status (0).
///
/// mirrors: `napi.rs::ok` -- `if let Some(env_ref) = env.as_mut() { .. }`: a
/// FRESH exclusive reborrow of the whole env, taken by every entry point of
/// the reference family on its way out. Whatever was derived from the env
/// before it is dead once this runs.
///
/// # Safety
///
/// `env` must be null or point to a live `StubEnv`, and the caller must not
/// use any reference derived from it earlier after this call.
pub unsafe fn ok(env: *mut StubEnv) -> u32 {
    // SAFETY: caller guarantees `env` is live or null; `as_mut` guards null.
    if let Some(env_ref) = unsafe { env.as_mut() } {
        env_ref.last_error = 0;
    }
    0
}

/// The body of `napi_reference_ref` (`delta` = +1) and `napi_reference_unref`
/// (`delta` = -1), statement for statement.
///
/// mirrors: both entry points --
/// 1. `env_scope_mut(env)`, taken BEFORE the entry is looked up and kept as
///    an `Option` (no native scope installed is not an error here);
/// 2. `ref_entry_mut(env, ref_)`, the last thing derived from the raw env
///    pointer until the status return;
/// 3. the saturating count update and the copy of the count;
/// 4. when the count crossed zero and a scope is installed,
///    `retarget_ref(scope, entry)` -- BOTH exclusive references in use at
///    once;
/// 5. `ok(env)`, a fresh `&mut *env`, after the entry's last use.
///
/// Returns the new count, or `None` for a handle that is not live in this
/// env (the `fail(env, NAPI_INVALID_ARG, ..)` return).
///
/// # Safety
///
/// `env` must point to a live `StubEnv` no reference is live over, whose
/// `scope` is null or points to a live `StubScope` nothing else is using.
pub unsafe fn reference_count_step(
    env: *mut StubEnv,
    handle: StubRefHandle,
    delta: i8,
) -> Option<u32> {
    // SAFETY: `env` is live per this function's contract; this is the only
    // scope reference taken here.
    let scope = unsafe { scope_exclusive(env) };
    // SAFETY: `env` is live and no reference derived from it is in use: the
    // scope above points at the scope object, not into the env.
    let Some(entry) = (unsafe { ref_entry_mut(env, handle) }) else {
        // SAFETY: `env` is live; nothing derived from it is used after this.
        // (`fail` writes the same slot `ok` does, with a non-zero status.)
        unsafe { (*env).last_error = 1 };
        return None;
    };
    entry.refcount = if delta >= 0 {
        entry.refcount.saturating_add(1)
    } else {
        entry.refcount.saturating_sub(1)
    };
    let count = entry.refcount;
    let crossed = if delta >= 0 { count == 1 } else { count == 0 };
    if crossed && let Some(scope) = scope {
        retarget_ref(scope, entry);
    }
    // SAFETY: `env` is live, and `entry` is not used again.
    unsafe { ok(env) };
    Some(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // Models of the CURRENT design. These must PASS under Miri; a failure
    // here is a real regression in the discipline napi.rs relies on.
    // ------------------------------------------------------------------

    /// Two shared reborrows of one stashed scope coexist, and the first stays
    /// usable after the second is created.
    ///
    /// mirrors: the #84 fix as it was made. `napi_define_class` took a scope
    /// with `env_scope`, called `napi_create_function` (which re-derived its
    /// own with `env_scope`), then used its own scope again to read
    /// `constructor.prototype`. With SHARED reborrows both are live at once.
    /// This is the exact shape that was UB before #84. That pair is gone
    /// from napi.rs since 2026-11 -- `napi_define_class` now takes ONE
    /// exclusive scope and hands it down to `create_function` by reborrow,
    /// which is safe Rust and needs no model -- so this stays as the check
    /// of the rule `env_scope` itself rests on: any number of shared
    /// reborrows of the stashed scope may be live together.
    #[test]
    fn two_shared_scopes_coexist() {
        let mut scope = StubScope { tick: 7 };
        let mut env = StubEnv::new();
        env.scope = &raw mut scope;
        let env_ptr: *mut StubEnv = &raw mut env;

        // SAFETY: `env_ptr` is live and its scope points to a live StubScope.
        let outer = unsafe { scope_shared(env_ptr) }.expect("outer scope");
        // The nested call re-derives from the UNCHANGED stashed pointer.
        // SAFETY: as above.
        let inner = unsafe { scope_shared(env_ptr) }.expect("inner scope");
        assert_eq!(inner.tick, 7);
        // ... and the outer reference is still usable afterwards.
        assert_eq!(outer.tick, 7);
    }

    /// A re-entrant install/restore keeps every frame's scope valid, because
    /// each native entry installs a DISTINCT scope object.
    ///
    /// mirrors: `napi.rs::napi_trampoline` -- save `prev`, install the
    /// current scope, run the addon callback, restore `prev`.
    #[test]
    fn reentrant_scope_swap_restores_each_frame() {
        let mut outer_scope = StubScope { tick: 1 };
        let mut inner_scope = StubScope { tick: 2 };
        let mut env = StubEnv::new();
        env.scope = &raw mut outer_scope;
        let env_ptr: *mut StubEnv = &raw mut env;

        // SAFETY: live env, live scope.
        let before = unsafe { scope_shared(env_ptr) }.expect("outer").tick;
        assert_eq!(before, 1);

        // Nested native entry: save, install a DIFFERENT scope, run, restore.
        // SAFETY: `env_ptr` is live for the whole block.
        unsafe {
            let prev = (*env_ptr).scope;
            (*env_ptr).scope = &raw mut inner_scope;
            let nested = scope_shared(env_ptr).expect("inner");
            assert_eq!(nested.tick, 2);
            (*env_ptr).scope = prev;
        }

        // SAFETY: live env; the outer scope is installed again.
        let after = unsafe { scope_shared(env_ptr) }.expect("outer again").tick;
        assert_eq!(after, 1, "the outer frame's scope must be restored");
    }

    /// The pointer an `External` holds to a boxed record survives the record
    /// table growing and other records leaving it, and the scope stays
    /// usable while the env is exclusively reborrowed.
    ///
    /// mirrors: `napi.rs::create_function` -- with its caller's scope in
    /// hand, reborrow `&mut *env`, file the boxed `FnData` in the id-keyed
    /// table, take `&raw mut **fn_data.entry(id).or_insert_with(..)` as the
    /// pointer handed to V8, then use the scope again (`External::new(scope,
    /// ..)`) with the `&mut` still live. The scope reference points at the
    /// scope object, not into the env, which is why the two do not conflict.
    /// `napi_trampoline` later reads the payload back through that pointer.
    /// A 2026-08-26 review claimed later insertions invalidate it (the table
    /// was a `Vec<Box<FnData>>` then); this is the machine check of that
    /// claim for the table as it is now. Growing the table moves the BOXES,
    /// not the heap `T`, and removing another record frees only that one.
    #[test]
    fn boxed_record_pointer_survives_table_growth() {
        let mut scope = StubScope { tick: 7 };
        let mut env = StubEnv::new();
        let mut collector = StubCollector::default();
        env.scope = &raw mut scope;
        let env_ptr: *mut StubEnv = &raw mut env;

        // SAFETY: `env_ptr` is live and its scope points to a live StubScope;
        // the shared reborrow inside `scope_exclusive` has ended by the time
        // the env is reborrowed, and nothing else borrows the env or uses
        // the scope.
        let (scope_ref, env_ref) = unsafe {
            (
                scope_exclusive(env_ptr).expect("the stashed scope"),
                &mut *env_ptr,
            )
        };

        let (_, handle) = env_ref.file_function(&mut collector, 11);
        // The scope is used while the exclusive env reborrow is still live.
        scope_ref.tick += 1;

        // Force the table to reallocate several times over, and take every
        // other record out again.
        let mut others = Vec::new();
        for n in 0..64u64 {
            others.push(env_ref.file_function(&mut collector, 100 + n).0);
        }
        for id in others.into_iter().step_by(2) {
            assert!(env_ref.fn_data.remove(&id).is_some());
        }

        // SAFETY: the box the handle names is still owned by `env.fn_data`;
        // table growth relocated the Box pointers, not the heap StubFnData.
        let fn_data = unsafe { &*handle };
        assert_eq!(fn_data.data, 11, "the dispensed pointer must still resolve");
        assert_eq!(scope.tick, 8);
    }

    /// A function record is released by id at a native entry after its
    /// `External` was collected -- never by the collector, and never under a
    /// call made through it.
    ///
    /// mirrors: `create_function` ([`StubEnv::file_function`]),
    /// `napi_trampoline` and `NapiEnv::take_collected`, in the order a
    /// running addon produces them:
    /// 1. two functions are created under one entry point's `&mut *env`;
    /// 2. the trampoline is entered for the first: it reads the record
    ///    through the `External`'s pointer ONCE, copying the fields out
    ///    (the shared reference ends with that statement), then runs
    ///    `run_pending_finalizers` -- a fresh `&mut *env` -- and the addon
    ///    callback;
    /// 3. the callback creates a function (growing the table), a collection
    ///    inside it reports the SECOND function's `External`, and a nested
    ///    native entry releases that record while the first call is still in
    ///    flight. The first function's own `External` cannot be reported
    ///    here: V8 holds it for the callback. The callback goes on using
    ///    what the trampoline copied;
    /// 4. after the call returns the first function's `External` is
    ///    collected, under a live entry-point borrow: the finalizer only
    ///    pushes the id, and the record is still there, still readable;
    /// 5. the next native entry takes the id and drops the record, once;
    /// 6. what was never collected drops with the env (the end of the
    ///    test: Miri reports a leak or a double free either way).
    #[test]
    fn function_record_is_released_by_id_and_never_under_a_call() {
        let mut env = StubEnv::new();
        let mut collector = StubCollector::default();
        let env_ptr: *mut StubEnv = &raw mut env;

        // 1. napi_define_class-like entry point: one borrow, two functions.
        // SAFETY: `env_ptr` is live and nothing else borrows the env; the
        // borrow is not used after the block.
        let ((first, first_record), (second, _)) = unsafe {
            let env_ref = &mut *env_ptr;
            (
                env_ref.file_function(&mut collector, 11),
                env_ref.file_function(&mut collector, 22),
            )
        };

        // 2 and 3: one call through the first function, start to finish.
        // SAFETY: `env_ptr` is live for the whole block and `env` is not
        // touched except through it; each reborrow of the env ends with its
        // own statement, as each entry point's does. `first_record` names a
        // record `env.fn_data` owns, whose External has not been collected,
        // and the read of it is over before anything else runs.
        let (data, third) = unsafe {
            // napi_trampoline: copy the fields out ...
            let data = (*first_record).data;
            // ... then run_pending_finalizers: nothing is owed yet.
            assert!((*env_ptr).take_collected().is_empty());

            // The addon callback. It creates a function ...
            let (third, _) = (*env_ptr).file_function(&mut collector, 33);
            // ... a collection reports the second function's External ...
            collector.collect(second);
            // ... and a nested native entry releases that record.
            let owed = (*env_ptr).take_collected();
            assert_eq!(owed.len(), 1, "the second function's tracking entry");
            (data, third)
        };
        // The running callback used what the trampoline copied, not the
        // record.
        assert_eq!(data, 11);

        // 4. The call has returned. A later entry point holds its borrow
        // while a collection reports the first function's External.
        // SAFETY: `env_ptr` is live and nothing else borrows the env.
        let env_ref = unsafe { &mut *env_ptr };
        let (fourth, _) = env_ref.file_function(&mut collector, 44);
        collector.collect(first);
        assert!(
            !env_ref.fn_data.contains_key(&second),
            "the nested entry released the second function's record"
        );
        assert!(
            env_ref.fn_data.contains_key(&first),
            "the collector only REPORTS; the record stays filed until the env takes it"
        );
        assert_eq!(env_ref.fn_data[&first].data, 11);

        // 5. The next native entry releases it -- once.
        // SAFETY: `env_ptr` is live, and the entry point's borrow above is
        // not used again; this one ends with the statement.
        assert_eq!(unsafe { &mut *env_ptr }.take_collected().len(), 1);
        assert!(!env.fn_data.contains_key(&first));
        assert!(env.take_collected().is_empty(), "handed over only once");

        // 6. The third and fourth were never collected: they go with the env.
        assert_eq!(env.fn_data.len(), 2);
        assert!(env.fn_data.contains_key(&third) && env.fn_data.contains_key(&fourth));
    }

    /// A handle resolves while its reference is live, and is refused once the
    /// reference is deleted -- including after its slot has been reused.
    ///
    /// mirrors: `napi_reference_ref` ([`reference_count_step`], here on its
    /// no-native-scope path: `env_scope_mut` yields `None`, the count is
    /// still updated and only the re-hold is skipped),
    /// `napi_get_reference_value` (shared lookup), `napi_delete_reference`
    /// (`free_ref`) and `napi_create_reference` (`alloc_ref`, which reuses
    /// the freed slot). The path WITH a scope, where the entry and the
    /// exclusive scope are used together, is
    /// `one_exclusive_scope_with_a_live_ref_entry`.
    /// Nothing here can be a use-after-free: a handle is an index plus a
    /// generation, and a stale one fails the generation compare.
    #[test]
    fn ref_entry_rejects_a_deleted_handle() {
        let mut env = StubEnv::new();
        let handle = env.alloc_ref(StubEntry {
            value: 42,
            refcount: 1,
            strong: true,
        });
        let env_ptr: *mut StubEnv = &raw mut env;

        // SAFETY: `env_ptr` is live for the whole block and `env` is not
        // touched except through it. Each reference derived from it is dead
        // before the next one is derived, which is the discipline under test.
        unsafe {
            // napi_reference_ref, with no scope installed on the env.
            let count = reference_count_step(env_ptr, handle, 1);
            assert_eq!(count, Some(2));
            assert_eq!((*env_ptr).last_error, 0);

            // napi_get_reference_value: a shared lookup.
            let entry = ref_entry(env_ptr, handle).expect("live handle");
            assert_eq!(entry.value, 42);

            // napi_delete_reference.
            let removed = (*env_ptr).free_ref(handle);
            assert_eq!(removed.map(|e| e.refcount), Some(2));
            let stale = ref_entry(env_ptr, handle);
            assert!(stale.is_none(), "a deleted handle must be refused");
            let again = (*env_ptr).free_ref(handle);
            assert!(again.is_none(), "and so must a double delete");
            // napi_reference_ref on the deleted handle: the `fail` return.
            assert_eq!(reference_count_step(env_ptr, handle, 1), None);
            assert_eq!((*env_ptr).last_error, 1);

            // napi_create_reference reuses the slot; the old handle names the
            // same index and must still miss.
            let reused = (*env_ptr).alloc_ref(StubEntry {
                value: 43,
                refcount: 1,
                strong: true,
            });
            assert_ne!(reused, handle);
            let recycled = ref_entry(env_ptr, handle);
            assert!(
                recycled.is_none(),
                "a stale handle must miss the slot's new owner"
            );
            assert_eq!(ref_entry(env_ptr, reused).map(|e| e.value), Some(43));
        }
    }

    /// A handle from a different env is refused: it names a live slot there,
    /// so only the per-env tag can catch it.
    ///
    /// mirrors: two addons loaded in one process, each with its own `NapiEnv`
    /// and its own `ref_tag`.
    #[test]
    fn ref_entry_rejects_another_envs_handle() {
        let mut env_a = StubEnv::with_tag(1);
        let handle_a = env_a.alloc_ref(StubEntry {
            value: 1,
            refcount: 1,
            strong: true,
        });

        let mut env_b = StubEnv::with_tag(2);
        env_b.alloc_ref(StubEntry {
            value: 2,
            refcount: 1,
            strong: true,
        });
        let env_b_ptr: *mut StubEnv = &raw mut env_b;

        // SAFETY: `env_b_ptr` is live; handle_a names index 0 at generation 1,
        // which is a live slot in env B too -- only the tag tells them apart.
        let foreign = unsafe { ref_entry(env_b_ptr, handle_a) };
        assert!(
            foreign.is_none(),
            "env B must refuse a handle env A dispensed"
        );
    }

    /// Deriving the env pointer AFTER the box is in place is sound.
    ///
    /// mirrors: `napi.rs::load_addon` -- a 2026-08-26 review claimed it derives
    /// the `Env` from a Box and THEN moves the Box, invalidating the pointer.
    /// This models the correct push-then-derive order the code should use, and
    /// `held_derive_then_move_box_is_ub` models the order the review alleged.
    #[test]
    // The push is deliberately separate from the Vec's creation: WHEN the box
    // enters the vec relative to when the pointer is derived is the whole
    // subject of this model, so `vec![..]` would erase it.
    #[allow(clippy::vec_init_then_push)]
    fn push_then_derive_env_pointer_is_sound() {
        let mut envs: Vec<Box<StubEnv>> = Vec::new();
        envs.push(Box::new(StubEnv::new()));
        let env_ptr: *mut StubEnv = &raw mut **envs.last_mut().unwrap();

        let mut scope = StubScope { tick: 5 };
        // SAFETY: `env_ptr` names the box just pushed, still owned by `envs`.
        unsafe { (*env_ptr).scope = &raw mut scope };

        // SAFETY: as above; the scope is live.
        let recovered = unsafe { scope_shared(env_ptr) }.expect("scope");
        assert_eq!(recovered.tick, 5);
    }

    /// ONE exclusive scope and a live exclusive entry are used together, and
    /// the env is reborrowed for the status return once both are done.
    ///
    /// mirrors: `napi_reference_unref` then `napi_reference_ref` on the same
    /// reference, each a full [`reference_count_step`] with a native scope
    /// installed: `env_scope_mut` first, `ref_entry_mut` second, then
    /// `retarget_ref(scope, entry)` as the count crosses zero (1 -> 0 goes
    /// weak, 0 -> 1 goes strong again), then `ok(env)`. This is the
    /// exclusive-scope discipline as the shipping code uses it. It is sound
    /// because the scope reference points at the scope object -- reading the
    /// stashed pointer out of the env leaves no borrow of the env behind --
    /// so the later `&mut` over the env (the lookup) and the scope do not
    /// overlap in memory.
    #[test]
    fn one_exclusive_scope_with_a_live_ref_entry() {
        let mut scope = StubScope { tick: 0 };
        let mut env = StubEnv::new();
        let handle = env.alloc_ref(StubEntry {
            value: 42,
            refcount: 1,
            strong: true,
        });
        env.scope = &raw mut scope;
        let env_ptr: *mut StubEnv = &raw mut env;

        // SAFETY: `env_ptr` is live with a live scope installed, and neither
        // `env` nor `scope` is touched except through it until the block
        // ends. Each step's references are dead when it returns.
        unsafe {
            // napi_reference_unref, 1 -> 0: re-held weakly.
            assert_eq!(reference_count_step(env_ptr, handle, -1), Some(0));
            let entry = ref_entry(env_ptr, handle).expect("live handle");
            assert!(!entry.strong, "a count of zero holds weakly");

            // napi_reference_unref again: saturates at zero, no crossing.
            assert_eq!(reference_count_step(env_ptr, handle, -1), Some(0));

            // napi_reference_ref, 0 -> 1: strong again.
            assert_eq!(reference_count_step(env_ptr, handle, 1), Some(1));
            let entry = ref_entry(env_ptr, handle).expect("live handle");
            assert!(entry.strong, "a positive count holds strongly");

            // 1 -> 2: no crossing, so the scope is left alone.
            assert_eq!(reference_count_step(env_ptr, handle, 1), Some(2));
            assert_eq!((*env_ptr).last_error, 0);
        }
        assert_eq!(scope.tick, 2, "the scope was used once per crossing");
    }

    /// A nested native entry takes its OWN exclusive scope while the outer
    /// entry point's exclusive scope is suspended, and the outer one is
    /// still usable when the nested entry has returned.
    ///
    /// mirrors: `catching` (as `napi_call_function` uses it) around
    /// `napi_trampoline`. The outer entry point takes `env_scope_mut` and
    /// calls into JS through it; JS calls an addon function, so V8 enters
    /// `napi_trampoline` with a `&mut PinScope` of its own -- a distinct
    /// scope object, built for that callback -- which the trampoline
    /// installs over the saved `prev`. The addon callback it runs calls an
    /// entry point that takes `env_scope_mut` in turn (and returns through
    /// `ok`). The trampoline restores `prev`, uses its own scope once more
    /// (re-throwing a pending exception), and returns; the outer entry point
    /// carries on with the reference it took at the start.
    ///
    /// Two exclusive scope references exist at once here, and it is sound
    /// for the reason the #84 shape was not: they point at DIFFERENT
    /// objects. That the two scopes are distinct objects is a fact about
    /// rusty_v8 (a callback gets a fresh `CallbackScope`) that this model
    /// takes as given; it cannot check it.
    #[test]
    fn nested_entry_takes_its_own_exclusive_scope() {
        let mut outer_scope = StubScope { tick: 10 };
        let mut inner_scope = StubScope { tick: 20 };
        let mut env = StubEnv::new();
        let handle = env.alloc_ref(StubEntry {
            value: 42,
            refcount: 1,
            strong: true,
        });
        env.scope = &raw mut outer_scope;
        let env_ptr: *mut StubEnv = &raw mut env;

        // The outer entry point (`catching`): its one exclusive scope.
        // SAFETY: `env_ptr` is live with the outer scope installed, and
        // nothing else refers to that scope.
        let outer = unsafe { scope_exclusive(env_ptr) }.expect("outer scope");
        outer.tick += 1;

        // ... calls into JS, which calls an addon function: napi_trampoline.
        {
            // The `scope: &mut PinScope` V8 hands the trampoline.
            let trampoline_scope: &mut StubScope = &mut inner_scope;
            // SAFETY: `env_ptr` is live. No reference into the env is held
            // across these statements: `outer` points at the outer scope
            // object, not into the env.
            unsafe {
                let prev = (*env_ptr).scope;
                (*env_ptr).scope = &raw mut *trampoline_scope;

                // The addon callback: an entry point that takes the
                // exclusive scope -- napi_reference_unref crossing zero,
                // scope and entry in use together, then `ok(env)`.
                assert_eq!(reference_count_step(env_ptr, handle, -1), Some(0));

                (*env_ptr).scope = prev;
            }
            // The trampoline's own scope, used again after the callback
            // (`v8::Local::new(scope, ..)` / `throw_exception`).
            trampoline_scope.tick += 1;
        }

        // Back in the outer entry point: its exclusive scope is intact.
        outer.tick += 1;
        assert_eq!(outer.tick, 12);
        // SAFETY: `env_ptr` is live; `outer` is not used again.
        unsafe { ok(env_ptr) };

        assert_eq!(inner_scope.tick, 22, "one re-hold, one trampoline use");
        assert_eq!(outer_scope.tick, 12);
        assert!(std::ptr::eq(env.scope, &raw const outer_scope));
        assert!(!env.refs[0].entry.as_ref().expect("live").strong);
    }

    /// A collection that happens while an entry point still holds its
    /// `&mut NapiEnv` does not disturb that borrow: the finalizer writes the
    /// id queue, a separate allocation it reaches through its own `Rc`, and
    /// never the env.
    ///
    /// mirrors: `napi_wrap` -- one `&mut *env` taken at the top and held
    /// across `NapiEnv::track_weak` (`Weak::with_finalizer`), `Number::new`
    /// and `set_private`, any of which may allocate and so collect, then
    /// used again (the undo path's `tracked.remove`, or the next statement).
    /// `napi_add_finalizer` and `create_function` hold theirs the same way. Then `run_pending_finalizers`: a FRESH `&mut *env` that takes the
    /// collected entries out whole and ends before any addon finalizer runs.
    #[test]
    fn collector_reports_by_id_under_a_live_env_borrow() {
        let mut env = StubEnv::new();
        let mut collector = StubCollector::default();
        let env_ptr: *mut StubEnv = &raw mut env;

        // SAFETY: `env_ptr` is live and nothing else borrows the env. This
        // is the entry point's one borrow, held to the end of the "call".
        let env_ref = unsafe { &mut *env_ptr };
        let kept = env_ref.track_weak(&mut collector, 0x10);
        let lost = env_ref.track_weak(&mut collector, 0x20);

        // A later allocating V8 call collects `lost` -- with `env_ref` live.
        collector.collect(lost);

        // ... and the entry point goes on using its borrow afterwards.
        env_ref.tracked.get_mut(&kept).expect("kept").native = 0x11;
        assert!(
            env_ref.tracked.contains_key(&lost),
            "the collector only REPORTS; the entry stays filed until the env takes it"
        );
        let next = env_ref.track_weak(&mut collector, 0x30);
        assert_eq!(next, lost + 1);

        // The next native entry: run_pending_finalizers' own reborrow.
        // SAFETY: `env_ptr` is live, and the entry point's borrow above is
        // not used again; this one ends with the statement.
        let owed = unsafe { &mut *env_ptr }.take_collected();
        assert_eq!(owed, [StubWrapEntry { native: 0x20 }]);
        assert!(!env.tracked.contains_key(&lost));
        assert_eq!(
            env.tracked.get(&kept),
            Some(&StubWrapEntry { native: 0x11 })
        );
        assert!(env.take_collected().is_empty(), "handed over only once");
    }

    // ------------------------------------------------------------------
    // The teeth. These model shapes that were REAL BUGS -- or, where a note
    // says so, the design a current model replaced -- and are expected to
    // be REJECTED by Miri, so they cannot be ordinary passing tests.
    //
    //   cargo +nightly miri test -p oam_aliasing_model -- --ignored
    //
    // Each must FAIL. If one of them starts passing, this harness has stopped
    // detecting the class it exists for -- treat that as the harness breaking,
    // not as good news.
    // ------------------------------------------------------------------

    /// The #84 bug: two EXCLUSIVE reborrows of one stashed scope, first used
    /// after the second is created.
    ///
    /// mirrors: pre-#84 `napi_define_class` taking its scope, then
    /// `napi_create_function` re-deriving one, then `napi_define_class`
    /// using its own again. Today it is also exactly what breaking
    /// `env_scope_mut`'s contract would be: an entry point that holds the
    /// exclusive scope and calls a helper that takes it again.
    /// Expected: Miri reports a Stacked/Tree Borrows violation on the
    /// `outer.tick` read.
    #[test]
    #[ignore = "models pre-#84 UB; expected to be REJECTED by Miri"]
    fn held_two_exclusive_scopes_is_ub() {
        let mut scope = StubScope { tick: 7 };
        let mut env = StubEnv::new();
        env.scope = &raw mut scope;
        let env_ptr: *mut StubEnv = &raw mut env;

        // SAFETY: deliberately unsound -- this is the bug being demonstrated.
        let outer = unsafe { scope_exclusive(env_ptr) }.expect("outer");
        // SAFETY: deliberately unsound; creating this pops `outer`.
        let inner = unsafe { scope_exclusive(env_ptr) }.expect("inner");
        inner.tick += 1;
        // Using `outer` after `inner` exists is the violation.
        assert_eq!(outer.tick, 8);
    }

    /// The #86 bug: dereference a handle whose entry has been freed.
    ///
    /// mirrors: pre-#86 `napi_get_reference_value`, which dereferenced the
    /// caller's raw handle behind a null check alone, when a `napi_ref` was
    /// still a pointer to a boxed entry (`refs: Vec<Box<NapiRefEntry>>`).
    /// The table here is that old design, kept local to this test: today's
    /// table (`StubEnv::refs`) hands out no pointer to free. Expected: Miri
    /// reports a use of a dangling pointer.
    #[test]
    #[ignore = "models pre-#86 use-after-free; expected to be REJECTED by Miri"]
    fn held_deref_deleted_handle_is_use_after_free() {
        let mut refs: Vec<Box<StubEntry>> = vec![Box::new(StubEntry {
            value: 42,
            refcount: 1,
            strong: true,
        })];
        let handle: *mut StubEntry = &raw mut **refs.last_mut().unwrap();

        let target = handle as *const StubEntry;
        refs.retain(|r| !std::ptr::eq(r.as_ref(), target));

        // SAFETY: deliberately unsound -- the box was just dropped. This is the
        // unvalidated deref #86 removed.
        let entry = unsafe { &*handle };
        assert_eq!(entry.value, 42);
    }

    /// The design the id queue replaced: a finalizer that captures the env
    /// POINTER and moves the entry itself (`object_collected`), run while an
    /// entry point holds a `&mut NapiEnv`.
    ///
    /// Never a shipped bug: while the finalizer worked this way, every
    /// helper in napi.rs re-derived `&mut *env` statement by statement so
    /// that no borrow was ever live across an allocating V8 call. This is
    /// what that file-wide rule was holding off, and why the current
    /// `napi_wrap` shape (see
    /// `collector_reports_by_id_under_a_live_env_borrow`) is only sound with
    /// the queue. The old two-field layout is kept local to this test.
    /// Expected: Miri reports a Stacked/Tree Borrows violation on the last
    /// use of `env_ref`.
    #[test]
    #[ignore = "models the replaced env-capturing finalizer; expected to be REJECTED by Miri"]
    fn held_collector_writing_through_the_env_is_ub() {
        struct OldEnv {
            tracked: std::collections::BTreeMap<usize, StubWrapEntry>,
            finalize_queue: Vec<StubWrapEntry>,
        }
        let mut env = OldEnv {
            tracked: std::collections::BTreeMap::new(),
            finalize_queue: Vec::new(),
        };
        let env_ptr: *mut OldEnv = &raw mut env;

        // SAFETY: `env_ptr` is live; the entry point's borrow.
        let env_ref = unsafe { &mut *env_ptr };
        env_ref.tracked.insert(1, StubWrapEntry { native: 0x20 });

        let finalizer: Box<dyn FnOnce()> = Box::new(move || {
            // SAFETY: deliberately unsound -- the collector reaching into the
            // env while the entry point's borrow of it is live.
            let collected_from = unsafe { &mut *env_ptr };
            if let Some(entry) = collected_from.tracked.remove(&1) {
                collected_from.finalize_queue.push(entry);
            }
        });
        // A collection inside an allocating V8 call the entry point makes.
        finalizer();

        // Using `env_ref` after the finalizer wrote through the raw pointer
        // is the violation.
        env_ref.tracked.insert(2, StubWrapEntry { native: 0x30 });
        assert_eq!(env_ref.finalize_queue.len(), 1);
    }

    /// An addon's saved env pointer stays usable after the registry that
    /// owns the env is gone.
    ///
    /// mirrors: `load_addon` (file the env, then derive the pointer the addon
    /// keeps), `AddonRegistry::envs` (re-derive from each box in place),
    /// `Drop for AddonRegistry` (strip through the re-derived pointer, free
    /// one env in place with `= None`, `mem::forget` the `Vec`), and then an
    /// entry point called late with the pointer the addon saved at load --
    /// `fail` / `ok` writing `last_error` through it.
    ///
    /// `mem::forget(v)` is `ManuallyDrop::new(v)`; the model keeps the
    /// `ManuallyDrop` so it can free everything at the end and stay clean
    /// under Miri's leak check.
    #[test]
    fn tombstone_outlives_its_registry() {
        struct StubAddon {
            env: Option<Box<StubEnv>>,
        }
        let mut addons: Vec<StubAddon> = Vec::new();

        // load_addon: push first, derive in place. This is the addon's copy.
        addons.push(StubAddon {
            env: Some(Box::new(StubEnv::new())),
        });
        let saved: *mut StubEnv = &raw mut **addons
            .last_mut()
            .expect("pushed")
            .env
            .as_mut()
            .expect("set");
        // Later loads grow the Vec, which moves its elements (not the boxes'
        // pointees).
        for _ in 0..8 {
            addons.push(StubAddon {
                env: Some(Box::new(StubEnv::new())),
            });
        }

        // AddonRegistry::envs, as teardown and the registry's drop call it.
        let envs: Vec<*mut StubEnv> = addons
            .iter_mut()
            .filter_map(|addon| addon.env.as_mut())
            .map(|env| &raw mut **env)
            .collect();
        for &env in &envs {
            // SAFETY: derived just above from a live box, no reference into
            // the env held: the strip, through `&mut *env`.
            let env_ref = unsafe { &mut *env };
            env_ref.scope = std::ptr::null_mut();
            env_ref.refs = Vec::new();
            env_ref.last_error = 0;
        }
        // One library really was unmapped: its env is freed where it is.
        addons[3].env = None;
        // The rest outlive the registry.
        let mut leaked = std::mem::ManuallyDrop::new(std::mem::take(&mut addons));

        // A library destructor presents the pointer it saved at load.
        // SAFETY: `saved` points at an env that was never freed and whose
        // box was never moved; whether the pointer itself survived the
        // re-derivations, the strip and the forget is what is under test.
        let status = unsafe {
            (*saved).last_error = 1;
            (*saved).last_error
        };
        assert_eq!(status, 1);
        assert!(
            leaked[0]
                .env
                .as_ref()
                .is_some_and(|env| env.last_error == 1)
        );

        // Not in napi.rs: reclaim what the real registry leaves allocated.
        drop(std::mem::ManuallyDrop::into_inner(std::mem::take(
            &mut leaked,
        )));
    }

    /// The shape a 2026-08-26 review alleged in `load_addon`: derive a raw
    /// pointer from a Box, then MOVE the Box, then use the pointer.
    ///
    /// Kept `#[ignore]`d and run deliberately because the verdict is what is
    /// interesting: whether Miri actually treats this as a violation decides
    /// whether that review finding was real. Recorded in the run notes rather
    /// than asserted here.
    #[test]
    #[ignore = "models an alleged load_addon defect; run to learn Miri's verdict"]
    // As above: the derive-then-move ORDER is the subject, so the push must
    // stay a separate statement.
    #[allow(clippy::vec_init_then_push)]
    fn held_derive_then_move_box_is_ub() {
        let mut boxed = Box::new(StubEnv::new());
        let env_ptr: *mut StubEnv = &raw mut *boxed;

        let mut envs: Vec<Box<StubEnv>> = Vec::new();
        envs.push(boxed); // the Box moves here

        let mut scope = StubScope { tick: 9 };
        // SAFETY: deliberately questionable -- the pointer was derived before
        // the Box moved. Whether this is UB is exactly what is under test.
        unsafe { (*env_ptr).scope = &raw mut scope };

        // SAFETY: as above.
        let recovered = unsafe { scope_shared(env_ptr) }.expect("scope");
        assert_eq!(recovered.tick, 9);
        assert_eq!(envs.len(), 1);
    }
}
