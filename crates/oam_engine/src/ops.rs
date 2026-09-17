//! The Promise <-> Future bridge.
//!
//! JS-side: `globalThis.oam` carries the built-in async surface (sleep,
//! readTextFile for now — the namespace grows with the op table). Each call
//! creates a V8 PromiseResolver, spawns the op future onto oam_core's tokio
//! runtime, and parks the resolver in the PendingOps slot keyed by op id.
//! Loop-side: `settle_completion` maps each OpCompletion back to its
//! resolver on the isolate thread (Done -> undefined, Text -> string,
//! Failed -> reject with Error).

use oam_core::{CoreRuntime, HandleKey, OpCompletion, OpId, OpOutcome};
use std::collections::HashMap;
use std::future::Future;

#[derive(Default)]
pub(crate) struct PendingOps(HashMap<OpId, v8::Global<v8::PromiseResolver>>);

impl PendingOps {
    fn park(&mut self, id: OpId, resolver: v8::Global<v8::PromiseResolver>) {
        self.0.insert(id, resolver);
    }
}

macro_rules! core_runtime {
    ($scope:expr) => {
        match $scope.get_slot::<CoreRuntime>() {
            Some(rt) => rt,
            None => {
                let msg = v8::String::new($scope, "internal: runtime not initialized").unwrap();
                let exc = v8::Exception::error($scope, msg);
                $scope.throw_exception(exc);
                return;
            }
        }
    };
}

macro_rules! core_runtime_mut {
    ($scope:expr) => {
        match $scope.get_slot_mut::<CoreRuntime>() {
            Some(rt) => rt,
            None => {
                let msg = v8::String::new($scope, "internal: runtime not initialized").unwrap();
                let exc = v8::Exception::error($scope, msg);
                $scope.throw_exception(exc);
                return;
            }
        }
    };
}

macro_rules! pending_ops_mut {
    ($scope:expr) => {
        match $scope.get_slot_mut::<PendingOps>() {
            Some(ops) => ops,
            None => {
                let msg = v8::String::new($scope, "internal: pending ops not initialized").unwrap();
                let exc = v8::Exception::error($scope, msg);
                $scope.throw_exception(exc);
                return;
            }
        }
    };
}

/// Install the `oam` namespace object onto the global.
pub(crate) fn install(scope: &mut v8::PinScope<'_, '_>, context: v8::Local<v8::Context>) {
    let global = context.global(scope);
    let oam = v8::Object::new(scope);

    let bindings: [(&str, v8::Local<v8::Function>); 2] = [
        ("sleep", v8::Function::new(scope, op_sleep).unwrap()),
        (
            "readTextFile",
            v8::Function::new(scope, op_read_text_file).unwrap(),
        ),
    ];
    for (name, function) in bindings {
        let key = v8::String::new(scope, name).unwrap();
        oam.set(scope, key.into(), function.into());
    }

    let version_key = v8::String::new(scope, "version").unwrap();
    let version = v8::String::new(scope, env!("CARGO_PKG_VERSION")).unwrap();
    oam.set(scope, version_key.into(), version.into());

    let oam_key = v8::String::new(scope, "oam").unwrap();
    global.set(scope, oam_key.into(), oam.into());

    // __oam: the internal op table consumed by js/bootstrap.js. Not public
    // API; the bootstrap wraps these in web-shaped surfaces (fetch, ...).
    let internal = v8::Object::new(scope);
    let internal_bindings: [(&str, v8::Local<v8::Function>); 19] = [
        ("fetch", v8::Function::new(scope, op_fetch).unwrap()),
        // A fetch whose dispatcher has a `connect.lookup` hook parks before
        // dialling a host name; JS runs the hook and resumes or drops it.
        (
            "fetchContinue",
            v8::Function::new(scope, op_fetch_continue).unwrap(),
        ),
        (
            "fetchAbandon",
            v8::Function::new(scope, op_fetch_abandon).unwrap(),
        ),
        (
            "fetchBodyRead",
            v8::Function::new(scope, op_fetch_body_read).unwrap(),
        ),
        (
            "fetchBodyCancel",
            v8::Function::new(scope, op_fetch_body_cancel).unwrap(),
        ),
        (
            "wsConnect",
            v8::Function::new(scope, op_ws_connect).unwrap(),
        ),
        ("wsSend", v8::Function::new(scope, op_ws_send).unwrap()),
        ("wsRecv", v8::Function::new(scope, op_ws_recv).unwrap()),
        ("wsClose", v8::Function::new(scope, op_ws_close).unwrap()),
        ("wsDrop", v8::Function::new(scope, op_ws_drop).unwrap()),
        // oam.fork() -- pre-warmed pool spawn.
        (
            "forkSpawn",
            v8::Function::new(scope, op_fork_spawn).unwrap(),
        ),
        // Record-replay ops (installed unconditionally; no-ops when mode=off).
        (
            "recordRng",
            v8::Function::new(scope, op_record_rng).unwrap(),
        ),
        (
            "replayRng",
            v8::Function::new(scope, op_replay_rng).unwrap(),
        ),
        (
            "recordDateNow",
            v8::Function::new(scope, op_record_date_now).unwrap(),
        ),
        (
            "replayDateNow",
            v8::Function::new(scope, op_replay_date_now).unwrap(),
        ),
        (
            "recordPerfNow",
            v8::Function::new(scope, op_record_perf_now).unwrap(),
        ),
        (
            "replayPerfNow",
            v8::Function::new(scope, op_replay_perf_now).unwrap(),
        ),
        (
            "replayGetMode",
            v8::Function::new(scope, op_replay_get_mode).unwrap(),
        ),
        // Generated -> source position remap for transpiled files
        // (Error.prepareStackTrace in js/bootstrap.js is the consumer).
        (
            "mapPosition",
            v8::Function::new(scope, op_map_position).unwrap(),
        ),
    ];
    for (name, function) in internal_bindings {
        let key = v8::String::new(scope, name).unwrap();
        internal.set(scope, key.into(), function.into());
    }
    let internal_key = v8::String::new(scope, "__oam").unwrap();
    global.set(scope, internal_key.into(), internal.into());
}

/// The shared shape of every async binding (node_ops fs natives ride it
/// too): create the promise, hand the op to the runtime through `spawn`,
/// park the resolver under the op id it returns, return the promise via `rv`.
fn spawn_with(
    scope: &mut v8::PinScope<'_, '_>,
    rv: &mut v8::ReturnValue<'_, v8::Value>,
    spawn: impl FnOnce(&mut CoreRuntime) -> OpId,
) {
    let Some(resolver) = v8::PromiseResolver::new(scope) else {
        let message = v8::String::new(scope, "failed to create promise").unwrap();
        let exception = v8::Exception::error(scope, message);
        scope.throw_exception(exception);
        return;
    };
    let promise = resolver.get_promise(scope);
    let resolver = v8::Global::new(scope, resolver);

    let id = spawn(core_runtime_mut!(scope));
    pending_ops_mut!(scope).park(id, resolver);

    rv.set(promise.into());
}

/// Spawn `op` and return its promise via `rv`.
pub(crate) fn spawn_op(
    scope: &mut v8::PinScope<'_, '_>,
    rv: &mut v8::ReturnValue<'_, v8::Value>,
    op: impl Future<Output = OpOutcome> + Send + 'static,
) {
    spawn_with(scope, rv, |core| core.spawn_op(op));
}

/// Like `spawn_op`, but the op belongs to a handle JS can `ref()` /
/// `unref()` -- a socket's read, a server's accept, the stdin read: it keeps
/// the event loop alive only while the handle is referenced, and the runtime
/// remembers it under `key` so a later flip reaches it while it is still
/// blocked in the OS (see `CoreRuntime::spawn_handle_op`).
pub(crate) fn spawn_handle_op(
    scope: &mut v8::PinScope<'_, '_>,
    rv: &mut v8::ReturnValue<'_, v8::Value>,
    key: HandleKey,
    op: impl Future<Output = OpOutcome> + Send + 'static,
) {
    spawn_with(scope, rv, |core| core.spawn_handle_op(key, op));
}

/// Like `spawn_op`, but the op does NOT keep the event loop alive. For
/// passive watchers whose trigger may never arrive.
pub(crate) fn spawn_op_unref(
    scope: &mut v8::PinScope<'_, '_>,
    rv: &mut v8::ReturnValue<'_, v8::Value>,
    op: impl Future<Output = OpOutcome> + Send + 'static,
) {
    spawn_with(scope, rv, |core| core.spawn_op_unref(op));
}

/// `__oam.mapPosition(file, line, column) -> [line, column] | null`:
/// generated -> source position through the loader's source-map registry.
/// `line` and `column` are 1-based on both sides (the JS `CallSite`
/// convention; the registry speaks 0-based columns, converted here). Null
/// when the file has no registered map or the position has no mapping --
/// the caller keeps the generated position.
fn op_map_position(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    rv.set(v8::null(scope).into());
    let Some(file) = args.get(0).to_string(scope) else {
        return;
    };
    let file = file.to_rust_string_lossy(scope);
    let line = args.get(1).uint32_value(scope).unwrap_or(0);
    let column = args.get(2).uint32_value(scope).unwrap_or(0);
    if line == 0 || column == 0 {
        return;
    }
    let Some((src_line, src_col)) = oam_loader::sourcemap::lookup(&file, line, column - 1) else {
        return;
    };
    let out = v8::Array::new(scope, 2);
    let line_v8 = v8::Number::new(scope, f64::from(src_line));
    let col_v8 = v8::Number::new(scope, f64::from(src_col + 1));
    out.set_index(scope, 0, line_v8.into());
    out.set_index(scope, 1, col_v8.into());
    rv.set(out.into());
}

fn op_sleep(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let ms = args.get(0).number_value(scope).unwrap_or(0.0);
    let ms = if ms.is_finite() && ms > 0.0 {
        ms as u64
    } else {
        0
    };
    spawn_op(scope, &mut rv, oam_core::ops::sleep(ms));
}

fn op_read_text_file(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = args.get(0).to_string(scope) else {
        let message = v8::String::new(scope, "readTextFile requires a path").unwrap();
        let exception = v8::Exception::type_error(scope, message);
        scope.throw_exception(exception);
        return;
    };
    let path = path.to_rust_string_lossy(scope);
    // Permission gate: read access for this path.
    if let Err(denial) = scope
        .get_slot::<std::sync::Arc<crate::permissions::Permissions>>()
        .cloned()
        .unwrap_or_default()
        .check_read(&path)
    {
        crate::node_ops::throw_permission_denied(scope, &denial);
        return;
    }
    spawn_op(scope, &mut rv, oam_core::ops::read_text_file(path));
}

fn op_fetch(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(wire) = args.get(0).to_string(scope) else {
        let message = v8::String::new(scope, "fetch op requires a request payload").unwrap();
        let exception = v8::Exception::type_error(scope, message);
        scope.throw_exception(exception);
        return;
    };
    let wire = wire.to_rust_string_lossy(scope);
    let request = match oam_core::ops::parse_fetch_request(&wire) {
        Ok(request) => request,
        Err(message) => {
            let message = v8::String::new(scope, &message).unwrap();
            let exception = v8::Exception::type_error(scope, message);
            scope.throw_exception(exception);
            return;
        }
    };
    // Net permission gate: check the hostname extracted from the URL.
    {
        let host = ada_url::Url::parse(&request.url, None)
            .ok()
            .map(|u| u.hostname().to_string())
            .unwrap_or_default();
        if let Err(denial) = scope
            .get_slot::<std::sync::Arc<crate::permissions::Permissions>>()
            .cloned()
            .unwrap_or_default()
            .check_net(&host)
        {
            crate::node_ops::throw_permission_denied(scope, &denial);
            return;
        }
    }
    let core = core_runtime!(scope);
    let transport = core.http_client();
    let bodies = core.bodies();
    let ids = core.body_ids();
    let outbound = core.outbound_bodies();
    let continuations = core.fetch_continuations();
    spawn_op(
        scope,
        &mut rv,
        oam_core::ops::fetch(transport, request, bodies, ids, outbound, continuations),
    );
}

/// `__oam.fetchContinue(token, answerJson)`: resume the fetch parked under
/// `token` with its lookup hook's answer (`{"ips": [...]}`). Settles like
/// `fetch`: a response, a failure, or the next hop's lookup request. The
/// `--permission` net check in `op_fetch` covers the initial URL only; a
/// redirect hop is not checked there either.
fn op_fetch_continue(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let token = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let Some(answer) = args.get(1).to_string(scope) else {
        let message = v8::String::new(scope, "fetchContinue requires a lookup answer").unwrap();
        let exception = v8::Exception::type_error(scope, message);
        scope.throw_exception(exception);
        return;
    };
    let answer = answer.to_rust_string_lossy(scope);
    let core = core_runtime!(scope);
    let bodies = core.bodies();
    let ids = core.body_ids();
    let continuations = core.fetch_continuations();
    spawn_op(
        scope,
        &mut rv,
        oam_core::ops::fetch_continue(token, answer, bodies, ids, continuations),
    );
}

/// `__oam.fetchAbandon(token)`, synchronous: drop the fetch parked under
/// `token` (its hook failed or the fetch was aborted). Returns whether it was
/// still parked; an untaken streamed request body is released with it.
fn op_fetch_abandon(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let token = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let continuations = core_runtime!(scope).fetch_continuations();
    let dropped = oam_core::ops::fetch_abandon(token, &continuations);
    rv.set(v8::Boolean::new(scope, dropped).into());
}

fn op_fetch_body_read(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let bodies = core_runtime!(scope).bodies();
    let cancelled = core_runtime!(scope).cancelled_bodies();
    let cancel_signal = core_runtime!(scope).body_cancel_signal();
    spawn_op(
        scope,
        &mut rv,
        oam_core::ops::fetch_body_read(bodies, cancelled, cancel_signal, handle),
    );
}

/// Synchronous: drop the stored body (connection closes). Safe to call
/// on an already-drained handle. A handle absent from the registry may have
/// a read IN FLIGHT (remove-await-reinsert); tombstone it so the returning
/// read drops the body instead of reviving it.
fn op_fetch_body_cancel(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let removed = core_runtime!(scope)
        .bodies()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&handle);
    if removed.is_none() {
        core_runtime!(scope)
            .cancelled_bodies()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(handle);
        // Wake the parked read so the cancel takes effect even when the peer
        // has simply stopped sending (chunk() would never resolve).
        core_runtime!(scope).body_cancel_signal().notify_waiters();
    }
}

fn op_ws_connect(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(wire) = args.get(0).to_string(scope) else {
        let message = v8::String::new(scope, "wsConnect requires a JSON payload").unwrap();
        let exception = v8::Exception::type_error(scope, message);
        scope.throw_exception(exception);
        return;
    };
    let wire = wire.to_rust_string_lossy(scope);
    let parsed: serde_json::Value = match serde_json::from_str(&wire) {
        Ok(v) => v,
        Err(e) => {
            let message =
                v8::String::new(scope, &format!("wsConnect: malformed payload: {e}")).unwrap();
            let exception = v8::Exception::type_error(scope, message);
            scope.throw_exception(exception);
            return;
        }
    };
    let url = parsed["url"].as_str().unwrap_or("").to_string();
    let protocols: Vec<String> = parsed["protocols"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    {
        let host = ada_url::Url::parse(&url, None)
            .ok()
            .map(|u| u.hostname().to_string())
            .unwrap_or_default();
        if let Err(denial) = scope
            .get_slot::<std::sync::Arc<crate::permissions::Permissions>>()
            .cloned()
            .unwrap_or_default()
            .check_net(&host)
        {
            crate::node_ops::throw_permission_denied(scope, &denial);
            return;
        }
    }
    let core = core_runtime!(scope);
    let registry = core.ws();
    let ids = core.body_ids();
    spawn_op(
        scope,
        &mut rv,
        oam_core::websocket::ws_connect(registry, ids, url, protocols),
    );
}

fn op_ws_send(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let is_binary = args.get(2).boolean_value(scope);
    let message = if is_binary {
        if let Ok(ab) = v8::Local::<v8::ArrayBufferView>::try_from(args.get(1)) {
            let len = ab.byte_length();
            let mut buf = vec![0u8; len];
            ab.copy_contents(&mut buf);
            tokio_tungstenite::tungstenite::Message::Binary(buf)
        } else {
            let text = args
                .get(1)
                .to_string(scope)
                .map(|s| s.to_rust_string_lossy(scope))
                .unwrap_or_default();
            tokio_tungstenite::tungstenite::Message::Binary(text.into_bytes())
        }
    } else {
        let text = args
            .get(1)
            .to_string(scope)
            .map(|s| s.to_rust_string_lossy(scope))
            .unwrap_or_default();
        tokio_tungstenite::tungstenite::Message::Text(text)
    };
    let registry = core_runtime!(scope).ws();
    if let Err(msg) = oam_core::websocket::ws_send_sync(&registry, handle, message) {
        let msg_v8 = v8::String::new(scope, &msg).unwrap();
        let exception = v8::Exception::error(scope, msg_v8);
        scope.throw_exception(exception);
    }
}

fn op_ws_recv(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let registry = core_runtime!(scope).ws();
    spawn_op(
        scope,
        &mut rv,
        oam_core::websocket::ws_recv(registry, handle),
    );
}

fn op_ws_close(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let code = args.get(1).number_value(scope).unwrap_or(1000.0) as u16;
    let reason = args
        .get(2)
        .to_string(scope)
        .map(|s| s.to_rust_string_lossy(scope))
        .unwrap_or_default();
    let registry = core_runtime!(scope).ws();
    spawn_op(
        scope,
        &mut rv,
        oam_core::websocket::ws_close(registry, handle, code, reason),
    );
}

fn op_ws_drop(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let registry = core_runtime!(scope).ws();
    oam_core::websocket::ws_drop(&registry, handle);
}

/// Settle one completed op against its parked resolver. Runs on the isolate
/// thread inside the event loop's TryCatch.
pub(crate) fn settle_completion(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    completion: OpCompletion,
) {
    // Inbound OS signal: no parked resolver (it was never a spawn_op). Emit the
    // named event on `process` so JS listeners fire; a present listener also
    // means the OS default was already suppressed at install time. If nothing
    // listens, emit is a benign no-op (a signal watcher can outlive its last
    // listener by a turn on the remove path).
    if completion.id == oam_core::SIGNAL_OP_ID {
        if let OpOutcome::Signal(name) = completion.outcome {
            crate::modules::emit_process_event(tc, &name, &[]);
        }
        return;
    }
    let resolver = tc
        .get_slot_mut::<PendingOps>()
        .and_then(|pending| pending.0.remove(&completion.id));
    let Some(resolver) = resolver else {
        // Op from a previous execute_module run whose map was reset: ignore.
        return;
    };
    let resolver = v8::Local::new(tc, &resolver);
    match completion.outcome {
        OpOutcome::Done => {
            let value: v8::Local<v8::Value> = v8::undefined(tc).into();
            resolver.resolve(tc, value);
        }
        OpOutcome::Text(text) => match v8::String::new(tc, &text) {
            Some(value) => {
                resolver.resolve(tc, value.into());
            }
            None => {
                let message = v8::String::new(tc, "op result too long for V8 string").unwrap();
                let exception = v8::Exception::error(tc, message);
                resolver.reject(tc, exception);
            }
        },
        OpOutcome::Json(json) => {
            let parsed = v8::String::new(tc, &json).and_then(|s| v8::json::parse(tc, s));
            match parsed {
                Some(value) => {
                    resolver.resolve(tc, value);
                }
                None => {
                    let message =
                        v8::String::new(tc, "op produced an unparseable payload").unwrap();
                    let exception = v8::Exception::error(tc, message);
                    resolver.reject(tc, exception);
                }
            }
        }
        OpOutcome::Bytes(bytes) => {
            let len = bytes.len();
            let store = v8::ArrayBuffer::new_backing_store_from_bytes(bytes.into_boxed_slice())
                .make_shared();
            let buffer = v8::ArrayBuffer::with_backing_store(tc, &store);
            match v8::Uint8Array::new(tc, buffer, 0, len) {
                Some(array) => {
                    resolver.resolve(tc, array.into());
                }
                None => {
                    let message =
                        v8::String::new(tc, "op produced an unviewable byte payload").unwrap();
                    let exception = v8::Exception::error(tc, message);
                    resolver.reject(tc, exception);
                }
            }
        }
        OpOutcome::Failed(message) => {
            let message = v8::String::new(tc, &message)
                .unwrap_or_else(|| v8::String::new(tc, "op failed").unwrap());
            let exception = v8::Exception::error(tc, message);
            resolver.reject(tc, exception);
        }
        OpOutcome::NodeFailed {
            code,
            message,
            syscall,
            path,
            errno,
            hostname,
            address,
            port,
        } => {
            let fields = SysFields {
                code: &code,
                message: &message,
                errno,
                syscall: syscall.as_deref(),
                path: path.as_deref(),
                hostname: hostname.as_deref(),
                address: address.as_deref(),
                port,
            };
            let error = sys_error(tc, &fields);
            let error = v8::Local::new(tc, &error);
            resolver.reject(tc, error);
        }
        OpOutcome::NodeAggregateFailed { errors } => {
            let error = aggregate_error(tc, &errors);
            let error = v8::Local::new(tc, &error);
            resolver.reject(tc, error);
        }
        // Handled by the SIGNAL_OP_ID early return above; a Signal outcome on a
        // non-signal id would be a logic error — drop it rather than resolve.
        OpOutcome::Signal(_) => {}
    }
}

/// One system error's fields, borrowed from a `NodeFailed` or a
/// `NodeSysError` (which has no path).
struct SysFields<'a> {
    code: &'a str,
    message: &'a str,
    errno: Option<i32>,
    syscall: Option<&'a str>,
    path: Option<&'a str>,
    hostname: Option<&'a str>,
    address: Option<&'a str>,
    port: Option<u16>,
}

impl<'a> SysFields<'a> {
    fn of(err: &'a oam_core::NodeSysError) -> Self {
        SysFields {
            code: &err.code,
            message: &err.message,
            errno: err.errno,
            syscall: err.syscall.as_deref(),
            path: None,
            hostname: err.hostname.as_deref(),
            address: err.address.as_deref(),
            port: err.port,
        }
    }
}

/// Look up one of the locked error factories bootstrap.js defines on the
/// global (`__oamMakeSysError` / `__oamMakeAggregateError`). They are
/// snapshotted JS, not part of `__oam` -- that object does not exist when the
/// snapshot is taken and ops::install replaces it after restore -- and they are
/// non-writable and non-configurable, so user code cannot swap them out. None
/// only if the global is somehow absent (a stripped build).
fn locked_factory<'s>(
    tc: &mut v8::PinnedRef<'s, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    name: &str,
) -> Option<v8::Local<'s, v8::Function>> {
    let context = tc.get_current_context();
    let global = context.global(tc);
    let key = v8::String::new(tc, name)?;
    v8::Local::<v8::Function>::try_from(global.get(tc, key.into())?).ok()
}

/// A factory call threw (or the lookup did): clear it so the settle path
/// leaves the loop's TryCatch as it found it. A termination is never cleared.
fn clear_factory_throw(tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>) {
    if tc.has_caught() && !tc.has_terminated() {
        tc.reset();
    }
}

/// Build the error a `NodeFailed` rejects with.
///
/// The JS factory `__oamMakeSysError` is preferred: it builds node's classes
/// (`ExceptionWithHostPort` for an address / port, `DNSException` for a
/// hostname, a plain Error otherwise) and, being JS, gives the connect and DNS
/// classes a stack frame. An error built here with no JS on the stack has none,
/// and both node's and oam's util.inspect then bracket it (`[Error: ...] {`)
/// where node prints a connect or DNS error unbracketed with its frames. The
/// plain-Error (fs) shape is left frameless by the factory on purpose: node's
/// fs callback errors have no frames either. The native build below is the
/// fallback, with the same own properties in the same order.
///
/// Property order is observable (`Object.keys(err)`): errno, code, syscall,
/// path, hostname, address, port -- errno FIRST, as on the sync path
/// (throw_node_error) and in node. Setting it last once gave async rejections
/// ["code","syscall","errno"]. `path` is absent (not empty) for an fd
/// operation (OpOutcome::node_failed_at); `port` only when non-zero, as node's
/// `if (port)`.
fn sys_error(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    fields: &SysFields<'_>,
) -> v8::Global<v8::Value> {
    let message = v8::String::new(tc, fields.message)
        .unwrap_or_else(|| v8::String::new(tc, fields.code).unwrap());
    let mut props: Vec<(&str, v8::Local<v8::Value>)> = Vec::with_capacity(7);
    // errno is a Number (node's negative libuv code), not a string.
    if let Some(errno) = fields.errno {
        props.push(("errno", v8::Integer::new(tc, errno).into()));
    }
    let strings = [
        ("code", Some(fields.code)),
        ("syscall", fields.syscall),
        ("path", fields.path),
        ("hostname", fields.hostname),
        ("address", fields.address),
    ];
    for (name, value) in strings {
        if let Some(value) = value.and_then(|v| v8::String::new(tc, v)) {
            props.push((name, value.into()));
        }
    }
    if let Some(port) = fields.port.filter(|port| *port != 0) {
        props.push((
            "port",
            v8::Integer::new_from_unsigned(tc, u32::from(port)).into(),
        ));
    }

    if let Some(factory) = locked_factory(tc, "__oamMakeSysError") {
        // A null-prototype record: the factory tests fields for presence, and
        // an Object.prototype a user script extended must not answer for an
        // absent one.
        let mut names: Vec<v8::Local<v8::Name>> = Vec::with_capacity(props.len() + 1);
        let mut values: Vec<v8::Local<v8::Value>> = Vec::with_capacity(props.len() + 1);
        names.push(v8::String::new(tc, "message").unwrap().into());
        values.push(message.into());
        for (name, value) in &props {
            names.push(v8::String::new(tc, name).unwrap().into());
            values.push(*value);
        }
        let null = v8::null(tc).into();
        let record = v8::Object::with_prototype_and_properties(tc, null, &names, &values);
        let recv = v8::undefined(tc).into();
        if let Some(error) = factory.call(tc, recv, &[record.into()]) {
            return v8::Global::new(tc, error);
        }
    }
    clear_factory_throw(tc);

    // The factory is locked, so the realistic way to get here is a user
    // accessor on Error.prototype (a throwing `code` setter, say) that threw
    // inside it. The own properties are therefore DEFINED, not assigned: an
    // assignment would run that same setter again and leave the error
    // half-shaped. On an untouched prototype the two are indistinguishable
    // (same order; enumerable, writable, configurable).
    let exception = v8::Exception::error(tc, message);
    if let Ok(obj) = v8::Local::<v8::Object>::try_from(exception) {
        for (name, value) in props {
            if let Some(key) = v8::String::new(tc, name) {
                obj.create_data_property(tc, key.into(), value);
            }
        }
    }
    // Defining on a fresh extensible Error does not throw; clear defensively
    // so the loop's TryCatch is never left holding anything. The promise is
    // rejected regardless.
    clear_factory_throw(tc);
    v8::Global::new(tc, exception)
}

/// Build the error a `NodeAggregateFailed` rejects with: node's
/// `NodeAggregateError` through `__oamMakeAggregateError`, each child through
/// `sys_error`. The fallback is a plain Error with no message, the children as
/// a non-enumerable own `errors` and the first child's `code` -- AggregateError's
/// observable surface without its class.
fn aggregate_error(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    errors: &[oam_core::NodeSysError],
) -> v8::Global<v8::Value> {
    let mut children: Vec<v8::Global<v8::Value>> = Vec::with_capacity(errors.len());
    for error in errors {
        children.push(sys_error(tc, &SysFields::of(error)));
    }
    let children: Vec<v8::Local<v8::Value>> = children
        .iter()
        .map(|child| v8::Local::new(tc, child))
        .collect();
    let array = v8::Array::new_with_elements(tc, &children);

    if let Some(factory) = locked_factory(tc, "__oamMakeAggregateError") {
        let recv = v8::undefined(tc).into();
        if let Some(error) = factory.call(tc, recv, &[array.into()]) {
            return v8::Global::new(tc, error);
        }
    }
    clear_factory_throw(tc);

    let empty = v8::String::empty(tc);
    let exception = v8::Exception::error(tc, empty);
    if let Ok(obj) = v8::Local::<v8::Object>::try_from(exception) {
        if let Some(key) = v8::String::new(tc, "errors") {
            obj.define_own_property(
                tc,
                key.into(),
                array.into(),
                v8::PropertyAttribute::DONT_ENUM,
            );
        }
        if let Some(first) = errors.first()
            && let (Some(key), Some(code)) = (
                v8::String::new(tc, "code"),
                v8::String::new(tc, &first.code),
            )
        {
            obj.create_data_property(tc, key.into(), code.into());
        }
    }
    clear_factory_throw(tc);
    v8::Global::new(tc, exception)
}

// ---------------------------------------------------------------------------
// oam.fork() -- pre-warmed pool spawn
// ---------------------------------------------------------------------------

/// Spawn a worker via the pre-warmed fork pool (or fall back to a cold spawn).
/// Returns the numeric worker_id synchronously; events arrive via
/// workerRecvMessage just like a regular worker_threads.Worker.
///
/// JS signature: `__oam.forkSpawn(scriptPath: string, workerData: string | null) -> number`
fn op_fork_spawn(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(script_path) = args.get(0).to_string(scope) else {
        let msg = v8::String::new(scope, "forkSpawn requires a script path").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    };
    let script_path = script_path.to_rust_string_lossy(scope);

    // oam.fork() starts a child ISOLATE -- same class as worker_threads, and
    // gated by the same permission. The child inherits `permissions` below,
    // so this is a gate on starting one at all, not on what it may do.
    if !crate::node_ops::check_worker_perm(scope, &script_path) {
        return;
    }
    let permissions = crate::node_ops::permissions_of(scope);

    let worker_data = if args.get(1).is_null_or_undefined() {
        None
    } else {
        args.get(1)
            .to_string(scope)
            .map(|s| s.to_rust_string_lossy(scope))
    };

    let path = std::path::PathBuf::from(&script_path);
    if !path.is_file() {
        let msg = v8::String::new(
            scope,
            &format!("forkSpawn: script not found: {script_path}"),
        )
        .unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }

    let workers = match scope.get_slot::<oam_core::CoreRuntime>() {
        Some(core) => core.workers(),
        None => {
            let msg = v8::String::new(scope, "internal: runtime not initialized").unwrap();
            let exc = v8::Exception::error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    };

    let worker_id = workers.lock().unwrap_or_else(|e| e.into_inner()).next_id();

    let (parent_to_worker_tx, parent_to_worker_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let (worker_to_parent_tx, worker_to_parent_rx) =
        std::sync::mpsc::channel::<oam_core::worker::WorkerEvent>();

    let pool = scope
        .get_slot::<std::sync::Arc<crate::fork::ForkPool>>()
        .cloned();

    match pool {
        Some(pool) => {
            pool.fork(
                path,
                worker_data,
                worker_id,
                worker_to_parent_tx,
                parent_to_worker_rx,
            );
        }
        None => {
            // Slot not set (e.g. worker thread that didn't initialize a pool):
            // fall back to a cold spawn.
            crate::worker::spawn_worker(
                path,
                worker_data,
                worker_id,
                parent_to_worker_rx,
                worker_to_parent_tx.clone(),
                crate::worker::WorkerOptions {
                    pipe_stdout: false,
                    pipe_stderr: false,
                    exec_argv: Vec::new(),
                },
                permissions,
            );
        }
    }

    {
        let mut guard = workers.lock().unwrap_or_else(|e| e.into_inner());
        guard.handles.insert(
            worker_id,
            oam_core::worker::WorkerHandle {
                to_worker: parent_to_worker_tx,
                thread: None,
            },
        );
        guard.receivers.insert(worker_id, worker_to_parent_rx);
    }

    rv.set(v8::Number::new(scope, worker_id as f64).into());
}

// ---------------------------------------------------------------------------
// Record-replay native ops
// ---------------------------------------------------------------------------

fn op_record_rng(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let value = args.get(0).number_value(scope).unwrap_or(0.0);
    if let Some(state) = scope.get_slot_mut::<crate::replay::ReplayState>()
        && let Some(recorder) = &mut state.recorder
    {
        let seq = recorder.next_seq();
        recorder.push(crate::replay::ReplayEvent::Rng { seq, value });
    }
    rv.set(args.get(0));
}

fn op_replay_rng(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    if let Some(state) = scope.get_slot_mut::<crate::replay::ReplayState>()
        && let Some(replayer) = &mut state.replayer
        && let Some(v) = replayer.next_rng()
    {
        rv.set(v8::Number::new(scope, v).into());
        return;
    }
    rv.set(args.get(0));
}

fn op_record_date_now(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let value = args.get(0).integer_value(scope).unwrap_or(0);
    if let Some(state) = scope.get_slot_mut::<crate::replay::ReplayState>()
        && let Some(recorder) = &mut state.recorder
    {
        let seq = recorder.next_seq();
        recorder.push(crate::replay::ReplayEvent::DateNow { seq, value });
    }
    rv.set(args.get(0));
}

fn op_replay_date_now(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    if let Some(state) = scope.get_slot_mut::<crate::replay::ReplayState>()
        && let Some(replayer) = &mut state.replayer
        && let Some(v) = replayer.next_date_now()
    {
        rv.set(v8::Number::new(scope, v as f64).into());
        return;
    }
    rv.set(args.get(0));
}

fn op_record_perf_now(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let value = args.get(0).number_value(scope).unwrap_or(0.0);
    if let Some(state) = scope.get_slot_mut::<crate::replay::ReplayState>()
        && let Some(recorder) = &mut state.recorder
    {
        let seq = recorder.next_seq();
        recorder.push(crate::replay::ReplayEvent::PerfNow { seq, value });
    }
    rv.set(args.get(0));
}

fn op_replay_perf_now(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    if let Some(state) = scope.get_slot_mut::<crate::replay::ReplayState>()
        && let Some(replayer) = &mut state.replayer
        && let Some(v) = replayer.next_perf_now()
    {
        rv.set(v8::Number::new(scope, v).into());
        return;
    }
    rv.set(args.get(0));
}

fn op_replay_get_mode(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let mode = scope
        .get_slot::<crate::replay::ReplayState>()
        .map(|s| s.mode_str())
        .unwrap_or("off");
    if let Some(s) = v8::String::new(scope, mode) {
        rv.set(s.into());
    }
}
