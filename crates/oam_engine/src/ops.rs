//! The Promise <-> Future bridge.
//!
//! JS-side: `globalThis.oam` carries the built-in async surface (sleep,
//! readTextFile for now — the namespace grows with the op table). Each call
//! creates a V8 PromiseResolver, spawns the op future onto oam_core's tokio
//! runtime, and parks the resolver in the PendingOps slot keyed by op id.
//! Loop-side: `settle_completion` maps each OpCompletion back to its
//! resolver on the isolate thread (Done -> undefined, Text -> string,
//! Failed -> reject with Error).

use oam_core::{CoreRuntime, OpCompletion, OpId, OpOutcome};
use std::collections::HashMap;
use std::future::Future;

#[derive(Default)]
pub(crate) struct PendingOps(HashMap<OpId, v8::Global<v8::PromiseResolver>>);

impl PendingOps {
    fn park(&mut self, id: OpId, resolver: v8::Global<v8::PromiseResolver>) {
        self.0.insert(id, resolver);
    }
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
    let internal_bindings: [(&str, v8::Local<v8::Function>); 8] = [
        ("fetch", v8::Function::new(scope, op_fetch).unwrap()),
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
    ];
    for (name, function) in internal_bindings {
        let key = v8::String::new(scope, name).unwrap();
        internal.set(scope, key.into(), function.into());
    }
    let internal_key = v8::String::new(scope, "__oam").unwrap();
    global.set(scope, internal_key.into(), internal.into());
}

/// Spawn `op` and return its promise via `rv`. The shared shape of every
/// async binding (node_ops fs natives ride it too).
pub(crate) fn spawn_op(
    scope: &mut v8::PinScope<'_, '_>,
    rv: &mut v8::ReturnValue<'_, v8::Value>,
    op: impl Future<Output = OpOutcome> + Send + 'static,
) {
    let Some(resolver) = v8::PromiseResolver::new(scope) else {
        let message = v8::String::new(scope, "failed to create promise").unwrap();
        let exception = v8::Exception::error(scope, message);
        scope.throw_exception(exception);
        return;
    };
    let promise = resolver.get_promise(scope);
    let resolver = v8::Global::new(scope, resolver);

    let id = scope
        .get_slot_mut::<CoreRuntime>()
        .expect("core runtime installed")
        .spawn_op(op);
    scope
        .get_slot_mut::<PendingOps>()
        .expect("pending ops installed")
        .park(id, resolver);

    rv.set(promise.into());
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
    if let Err(msg) = scope
        .get_slot::<crate::permissions::Permissions>()
        .cloned()
        .unwrap_or_default()
        .check_read(&path)
    {
        let msg_v8 = v8::String::new(scope, &msg)
            .unwrap_or_else(|| v8::String::new(scope, "ERR_PERMISSION_DENIED").unwrap());
        let exception = v8::Exception::error(scope, msg_v8);
        if let Ok(obj) = v8::Local::<v8::Object>::try_from(exception) {
            let key = v8::String::new(scope, "code").unwrap();
            if let Some(code) = v8::String::new(scope, "ERR_PERMISSION_DENIED") {
                obj.set(scope, key.into(), code.into());
            }
        }
        scope.throw_exception(exception);
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
        if let Err(msg) = scope
            .get_slot::<crate::permissions::Permissions>()
            .cloned()
            .unwrap_or_default()
            .check_net(&host)
        {
            let msg_v8 = v8::String::new(scope, &msg)
                .unwrap_or_else(|| v8::String::new(scope, "ERR_PERMISSION_DENIED").unwrap());
            let exception = v8::Exception::error(scope, msg_v8);
            if let Ok(obj) = v8::Local::<v8::Object>::try_from(exception) {
                let key = v8::String::new(scope, "code").unwrap();
                if let Some(code) = v8::String::new(scope, "ERR_PERMISSION_DENIED") {
                    obj.set(scope, key.into(), code.into());
                }
            }
            scope.throw_exception(exception);
            return;
        }
    }
    let core = scope
        .get_slot::<CoreRuntime>()
        .expect("core runtime installed");
    let client = core.http_client();
    let bodies = core.bodies();
    let ids = core.body_ids();
    spawn_op(
        scope,
        &mut rv,
        oam_core::ops::fetch(client, request, bodies, ids),
    );
}

fn op_fetch_body_read(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let bodies = scope
        .get_slot::<CoreRuntime>()
        .expect("core runtime installed")
        .bodies();
    spawn_op(
        scope,
        &mut rv,
        oam_core::ops::fetch_body_read(bodies, handle),
    );
}

/// Synchronous: drop the stored response (connection closes). Safe to call
/// on an already-drained handle.
fn op_fetch_body_cancel(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let bodies = scope
        .get_slot::<CoreRuntime>()
        .expect("core runtime installed")
        .bodies();
    bodies.lock().unwrap_or_else(|e| e.into_inner()).remove(&handle);
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
        if let Err(msg) = scope
            .get_slot::<crate::permissions::Permissions>()
            .cloned()
            .unwrap_or_default()
            .check_net(&host)
        {
            let msg_v8 = v8::String::new(scope, &msg)
                .unwrap_or_else(|| v8::String::new(scope, "ERR_PERMISSION_DENIED").unwrap());
            let exception = v8::Exception::error(scope, msg_v8);
            if let Ok(obj) = v8::Local::<v8::Object>::try_from(exception) {
                let key = v8::String::new(scope, "code").unwrap();
                if let Some(code) = v8::String::new(scope, "ERR_PERMISSION_DENIED") {
                    obj.set(scope, key.into(), code.into());
                }
            }
            scope.throw_exception(exception);
            return;
        }
    }
    let core = scope
        .get_slot::<CoreRuntime>()
        .expect("core runtime installed");
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
    let registry = scope
        .get_slot::<CoreRuntime>()
        .expect("core runtime installed")
        .ws();
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
    let registry = scope
        .get_slot::<CoreRuntime>()
        .expect("core runtime installed")
        .ws();
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
    let registry = scope
        .get_slot::<CoreRuntime>()
        .expect("core runtime installed")
        .ws();
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
    let registry = scope
        .get_slot::<CoreRuntime>()
        .expect("core runtime installed")
        .ws();
    oam_core::websocket::ws_drop(&registry, handle);
}

/// Settle one completed op against its parked resolver. Runs on the isolate
/// thread inside the event loop's TryCatch.
pub(crate) fn settle_completion(
    tc: &mut v8::PinnedRef<'_, v8::TryCatch<'_, '_, v8::HandleScope<'_>>>,
    completion: OpCompletion,
) {
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
        OpOutcome::NodeFailed { code, message } => {
            let message = v8::String::new(tc, &message)
                .unwrap_or_else(|| v8::String::new(tc, &code).unwrap());
            let exception = v8::Exception::error(tc, message);
            if let Ok(obj) = v8::Local::<v8::Object>::try_from(exception) {
                let key = v8::String::new(tc, "code").unwrap();
                if let Some(value) = v8::String::new(tc, &code) {
                    obj.set(tc, key.into(), value.into());
                }
            }
            resolver.reject(tc, exception);
        }
    }
}
