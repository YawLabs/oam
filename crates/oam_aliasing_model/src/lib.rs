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
//!   `mirrors:` note on each model naming the exact function and lines it was
//!   read from, and it is only as good as those notes. If `napi.rs` changes,
//!   a model can silently go stale -- so treat a note as a claim to re-check,
//!   not as a guarantee.
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

/// Stands in for `NapiEnv`. Only the two fields the models exercise:
/// the stashed scope pointer and an owned collection handed out by raw pointer.
#[derive(Debug)]
pub struct StubEnv {
    /// Mirrors `NapiEnv::scope`: `*mut c_void` holding a `*mut PinScope`,
    /// non-null only while a native entry is on the stack.
    pub scope: *mut StubScope,
    /// Mirrors `NapiEnv::refs`: `Vec<Box<NapiRefEntry>>`, whose elements are
    /// handed to addons as raw pointers.
    pub refs: Vec<Box<StubEntry>>,
}

/// Stands in for `NapiRefEntry`.
#[derive(Debug)]
pub struct StubEntry {
    pub value: u64,
    pub refcount: u32,
}

impl StubEnv {
    pub fn new() -> Self {
        Self {
            scope: std::ptr::null_mut(),
            refs: Vec::new(),
        }
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

/// Recover the stashed scope as an EXCLUSIVE reference -- the pre-#84 shape.
///
/// mirrors: `napi.rs::env_scope` BEFORE #84, which returned
/// `Option<&'a mut v8::PinScope<'static, 'static>>` via `as_mut`.
/// Kept only so `held_two_exclusive_scopes_is_ub` can demonstrate the bug.
///
/// # Safety
///
/// As [`scope_shared`]. Deriving two live `&mut` from one stashed pointer is
/// UB; that is the point of keeping this.
pub unsafe fn scope_exclusive<'a>(env: *mut StubEnv) -> Option<&'a mut StubScope> {
    // SAFETY: caller guarantees `env` is live or null; `as_mut` guards null.
    unsafe {
        let env_ref = env.as_mut()?;
        (env_ref.scope).as_mut()
    }
}

// ================================================================ references

/// Validate a handle against the env's own table before dereferencing it.
///
/// mirrors: `napi.rs::ref_entry` as introduced in #86 -- the `ptr::eq` identity
/// scan over `env.refs`, then the deref only on a hit.
///
/// # Safety
///
/// `env` must point to a live `StubEnv`. `handle` may be null, stale, or
/// foreign: rejecting those is what this models.
pub unsafe fn ref_entry<'a>(
    env: *mut StubEnv,
    handle: *mut StubEntry,
) -> Option<&'a mut StubEntry> {
    // SAFETY: caller guarantees `env` is live or null; the shared reborrow only
    // reads `refs` to validate the handle.
    let env_ref = unsafe { env.as_ref()? };
    let target = handle as *const StubEntry;
    if !env_ref
        .refs
        .iter()
        .any(|r| std::ptr::eq(r.as_ref(), target))
    {
        return None;
    }
    // SAFETY: `target` was just found in `refs`, so it names a live box owned
    // by this env; the box is heap-stable, so the address is valid.
    Some(unsafe { &mut *handle })
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
    /// mirrors: the #84 fix. `napi_define_class:1754` takes a scope, calls
    /// `napi_create_function` (which re-derives at `:1166`), then uses its own
    /// scope again at `:1775`. With SHARED reborrows both are live at once.
    /// This is the exact shape that was UB before #84.
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
    /// mirrors: `napi.rs::napi_trampoline:235-243` -- save `prev`, install the
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

    /// A raw pointer into a `Vec<Box<T>>` element survives later pushes.
    ///
    /// mirrors: `napi.rs::napi_create_reference` -- push the box, then take
    /// `&mut **refs.last_mut().unwrap()` as the dispensed handle. A 2026-08-26
    /// review claimed later pushes invalidate it; this is the machine check of
    /// that claim. Growing the Vec moves the BOXES, not the heap `T`.
    #[test]
    fn boxed_element_pointer_survives_vec_growth() {
        let mut env = StubEnv::new();
        env.refs.push(Box::new(StubEntry {
            value: 11,
            refcount: 1,
        }));
        let handle: *mut StubEntry = &raw mut **env.refs.last_mut().unwrap();

        // Force reallocation several times over.
        for n in 0..64u64 {
            env.refs.push(Box::new(StubEntry {
                value: 100 + n,
                refcount: 1,
            }));
        }

        // SAFETY: the box the handle names is still owned by `env.refs`; Vec
        // growth relocated the Box pointers, not the heap StubEntry.
        let entry = unsafe { &mut *handle };
        assert_eq!(entry.value, 11, "the dispensed handle must still resolve");
        entry.refcount += 1;
        assert_eq!(entry.refcount, 2);
    }

    /// Validate-then-deref accepts a live handle and rejects a deleted one.
    ///
    /// mirrors: `napi.rs::ref_entry` plus `napi_delete_reference`'s `retain`.
    /// This is the #86 use-after-free fix.
    #[test]
    fn ref_entry_rejects_a_deleted_handle() {
        let mut env = StubEnv::new();
        env.refs.push(Box::new(StubEntry {
            value: 42,
            refcount: 1,
        }));
        let handle: *mut StubEntry = &raw mut **env.refs.last_mut().unwrap();
        let env_ptr: *mut StubEnv = &raw mut env;

        // Live handle resolves.
        // SAFETY: `env_ptr` is live; ref_entry validates before dereferencing.
        let entry = unsafe { ref_entry(env_ptr, handle) }.expect("live handle");
        assert_eq!(entry.value, 42);

        // Delete it, exactly as napi_delete_reference does.
        let target = handle as *const StubEntry;
        // SAFETY: `env_ptr` is live.
        unsafe {
            (*env_ptr)
                .refs
                .retain(|r| !std::ptr::eq(r.as_ref(), target))
        };

        // The stale handle must be REFUSED, not dereferenced. Without the scan
        // this deref is a use-after-free and Miri rejects the test.
        // SAFETY: `env_ptr` is live; the handle is stale, which ref_entry
        // detects by identity rather than by dereferencing.
        let stale = unsafe { ref_entry(env_ptr, handle) };
        assert!(stale.is_none(), "a deleted handle must be refused");
    }

    /// A handle from a different env is refused: it is LIVE memory, so only the
    /// per-env identity scan can catch it.
    ///
    /// mirrors: two addons loaded in one process, each with its own `NapiEnv`.
    #[test]
    fn ref_entry_rejects_another_envs_handle() {
        let mut env_a = StubEnv::new();
        env_a.refs.push(Box::new(StubEntry {
            value: 1,
            refcount: 1,
        }));
        let handle_a: *mut StubEntry = &raw mut **env_a.refs.last_mut().unwrap();

        let mut env_b = StubEnv::new();
        env_b.refs.push(Box::new(StubEntry {
            value: 2,
            refcount: 1,
        }));
        let env_b_ptr: *mut StubEnv = &raw mut env_b;

        // SAFETY: `env_b_ptr` is live; handle_a is live memory owned by env_a,
        // which is precisely why only an identity scan can reject it.
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

    // ------------------------------------------------------------------
    // The teeth. These model shapes that were REAL BUGS and are expected to
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
    /// mirrors: pre-#84 `napi_define_class:1754`, then `napi_create_function`
    /// re-deriving at `:1166`, then reuse of the first scope at `:1775`.
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
    /// mirrors: pre-#86 `napi_get_reference_value:1630`, which dereferenced the
    /// caller's raw handle behind a null check alone. Expected: Miri reports a
    /// use of a dangling pointer.
    #[test]
    #[ignore = "models pre-#86 use-after-free; expected to be REJECTED by Miri"]
    fn held_deref_deleted_handle_is_use_after_free() {
        let mut env = StubEnv::new();
        env.refs.push(Box::new(StubEntry {
            value: 42,
            refcount: 1,
        }));
        let handle: *mut StubEntry = &raw mut **env.refs.last_mut().unwrap();

        let target = handle as *const StubEntry;
        env.refs.retain(|r| !std::ptr::eq(r.as_ref(), target));

        // SAFETY: deliberately unsound -- the box was just dropped. This is the
        // unvalidated deref #86 removed.
        let entry = unsafe { &*handle };
        assert_eq!(entry.value, 42);
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
