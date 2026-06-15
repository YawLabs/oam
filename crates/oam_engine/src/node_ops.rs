//! Native bindings for the node: compat layer: `__oam.node`.
//!
//! The JS half (js/node_compat.js factories) is pure and lives in the
//! snapshot; everything here is installed after restore. Sync fs natives
//! call std::fs directly on the isolate thread (that is what Sync means);
//! async fs natives ride the oam_core op channel like fetch does. All fs
//! failures throw/reject Errors carrying Node's `.code` (ENOENT, ...) —
//! the ecosystem branches on codes, not messages.

use crate::crypto_ops::{
    op_crypto_cipher_create, op_crypto_cipher_final, op_crypto_cipher_final_gcm,
    op_crypto_cipher_get_auth_tag, op_crypto_cipher_set_aad, op_crypto_cipher_set_auth_tag,
    op_crypto_cipher_set_auto_padding, op_crypto_cipher_update, op_crypto_hash_copy,
    op_crypto_hash_create, op_crypto_hash_digest, op_crypto_hash_update, op_crypto_hkdf_sync,
    op_crypto_hmac_create, op_crypto_pbkdf2_sync, op_crypto_random_fill, op_crypto_scrypt_sync,
    op_crypto_timing_safe_equal, op_crypto_sign, op_crypto_verify, op_crypto_generate_keypair,
    op_crypto_public_encrypt, op_crypto_private_decrypt,
    op_crypto_ecdh_generate_keys, op_crypto_ecdh_compute_secret, op_crypto_ecdh_get_public_key,
    op_crypto_dh_generate_keys, op_crypto_dh_compute_secret,
};
use oam_core::{node_error_code, node_error_message};
use std::path::PathBuf;
use std::time::Instant;

/// Process-start anchor for performance.now() / process.uptime().
static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

pub(crate) fn install(scope: &mut v8::PinScope<'_, '_>, context: v8::Local<v8::Context>) {
    START.get_or_init(Instant::now);

    let global = context.global(scope);
    let internal_key = v8::String::new(scope, "__oam").unwrap();
    let internal = global
        .get(scope, internal_key.into())
        .expect("__oam installed by ops::install");
    let internal = v8::Local::<v8::Object>::try_from(internal).expect("__oam is an object");

    let node = v8::Object::new(scope);

    // Constant data properties.
    let platform = match std::env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    };
    let data: [(&str, v8::Local<v8::Value>); 6] = [
        ("platform", v8::String::new(scope, platform).unwrap().into()),
        ("arch", v8::String::new(scope, arch).unwrap().into()),
        (
            "oamVersion",
            v8::String::new(scope, env!("CARGO_PKG_VERSION"))
                .unwrap()
                .into(),
        ),
        (
            "v8Version",
            v8::String::new(scope, v8::VERSION_STRING).unwrap().into(),
        ),
        (
            "pid",
            v8::Number::new(scope, std::process::id() as f64).into(),
        ),
        (
            "ppid",
            v8::Number::new(scope, parent_pid() as f64).into(),
        ),
    ];
    for (name, value) in data {
        let key = v8::String::new(scope, name).unwrap();
        node.set(scope, key.into(), value);
    }
    let cpu_count = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let key = v8::String::new(scope, "cpuCount").unwrap();
    let value = v8::Number::new(scope, cpu_count as f64);
    node.set(scope, key.into(), value.into());

    // Each binding must be a distinct fn ITEM (zero-sized) — rusty_v8's
    // MapFnTo rejects fn POINTERS, so no table-driven loop here.
    macro_rules! bind {
        ($(($name:literal, $f:ident)),+ $(,)?) => {
            $(
                let key = v8::String::new(scope, $name).unwrap();
                let func = v8::Function::new(scope, $f).unwrap();
                node.set(scope, key.into(), func.into());
            )+
        };
    }
    bind!(
        ("env", op_env),
        ("argv", op_argv),
        ("cwd", op_cwd),
        ("chdir", op_chdir),
        ("exit", op_exit),
        ("stdoutWrite", op_stdout_write),
        ("stderrWrite", op_stderr_write),
        ("isTTY", op_is_tty),
        ("nowMs", op_now_ms),
        ("hrtimeNanos", op_hrtime_nanos),
        ("uptimeMs", op_uptime_ms),
        ("homedir", op_homedir),
        ("tmpdir", op_tmpdir),
        ("hostname", op_hostname),
        ("username", op_username),
        ("makeRequire", op_make_require),
        // WHATWG URL (ada parser, same as Node.js): parse + mutation.
        // urlParseHref: validate + return canonical href (no component extraction).
        // urlParse: full extraction (called lazily on first property access).
        ("urlParseHref", op_url_parse_href),
        ("urlCanParse", op_url_can_parse),
        ("urlParse", op_url_parse),
        ("urlUpdate", op_url_update),
        // AsyncLocalStorage substrate: V8's continuation-preserved embedder
        // data, propagated across promise continuations by V8 itself.
        ("getContinuationData", op_get_continuation_data),
        ("setContinuationData", op_set_continuation_data),
        // fs sync
        ("fsReadFileSync", op_fs_read_file_sync),
        ("fsReadFileUtf8Sync", op_fs_read_file_utf8_sync),
        ("fsWriteFileSync", op_fs_write_file_sync),
        ("fsExistsSync", op_fs_exists_sync),
        ("fsStatSync", op_fs_stat_sync),
        ("fsReaddirSync", op_fs_readdir_sync),
        ("fsMkdirSync", op_fs_mkdir_sync),
        ("fsRmSync", op_fs_rm_sync),
        ("fsRenameSync", op_fs_rename_sync),
        ("fsCopyFileSync", op_fs_copy_file_sync),
        ("fsUnlinkSync", op_fs_unlink_sync),
        ("fsAccessSync", op_fs_access_sync),
        ("fsRealpathSync", op_fs_realpath_sync),
        // fs async (promise-returning, settled by the event loop)
        ("fsReadFile", op_fs_read_file),
        ("fsWriteFile", op_fs_write_file),
        ("fsStat", op_fs_stat),
        ("fsReaddir", op_fs_readdir),
        ("fsMkdir", op_fs_mkdir),
        ("fsRm", op_fs_rm),
        ("fsRename", op_fs_rename),
        ("fsCopyFile", op_fs_copy_file),
        ("fsUnlink", op_fs_unlink),
        ("fsAccess", op_fs_access),
        ("fsRealpath", op_fs_realpath),
        ("fsMkdtemp", op_fs_mkdtemp),
        ("fsSymlink", op_fs_symlink),
        ("fsReadlink", op_fs_readlink),
        ("fsLink", op_fs_link),
        ("fsChmod", op_fs_chmod),
        ("fsTruncate", op_fs_truncate),
        // fs sync (new batch)
        ("fsSymlinkSync", op_fs_symlink_sync),
        ("fsReadlinkSync", op_fs_readlink_sync),
        ("fsLinkSync", op_fs_link_sync),
        ("fsChmodSync", op_fs_chmod_sync),
        ("fsTruncateSync", op_fs_truncate_sync),
        ("fsMkdtempSync", op_fs_mkdtemp_sync),
        // fs streams (createReadStream/createWriteStream)
        ("fsOpen", op_fs_open),
        ("fsReadChunk", op_fs_read_chunk),
        ("fsWriteChunk", op_fs_write_chunk),
        ("fsClose", op_fs_close),
        // node:zlib (one-shot)
        ("zlibSync", op_zlib_sync),
        ("zlibAsync", op_zlib_async),
        // node:zlib streaming (incremental Transform backing)
        ("zlibStreamCreate", op_zlib_stream_create),
        ("zlibStreamWrite", op_zlib_stream_write),
        ("zlibStreamFlush", op_zlib_stream_flush),
        ("zlibStreamClose", op_zlib_stream_close),
        // HTTP server
        ("httpServe", op_http_serve),
        ("httpAccept", op_http_accept),
        ("httpRequestBody", op_http_request_body),
        ("httpRespond", op_http_respond),
        ("httpRespondStream", op_http_respond_stream),
        ("httpBodyPush", op_http_body_push),
        ("httpBodyEnd", op_http_body_end),
        ("httpClose", op_http_close),
        // TCP sockets (node:net)
        ("tcpConnect", op_tcp_connect),
        ("tcpRead", op_tcp_read),
        ("tcpWrite", op_tcp_write),
        ("tcpClose", op_tcp_close),
        ("tcpShutdown", op_tcp_shutdown),
        ("tcpListen", op_tcp_listen),
        ("tcpAccept", op_tcp_accept),
        ("tcpServerClose", op_tcp_server_close),
        // node:crypto (crypto_ops.rs)
        ("cryptoHashCreate", op_crypto_hash_create),
        ("cryptoHmacCreate", op_crypto_hmac_create),
        ("cryptoHashUpdate", op_crypto_hash_update),
        ("cryptoHashDigest", op_crypto_hash_digest),
        ("cryptoHashCopy", op_crypto_hash_copy),
        ("cryptoRandomFill", op_crypto_random_fill),
        ("cryptoTimingSafeEqual", op_crypto_timing_safe_equal),
        // node:crypto wave 2: key derivation + symmetric ciphers
        ("cryptoPbkdf2Sync", op_crypto_pbkdf2_sync),
        ("cryptoScryptSync", op_crypto_scrypt_sync),
        ("cryptoHkdfSync", op_crypto_hkdf_sync),
        ("cryptoCipherCreate", op_crypto_cipher_create),
        ("cryptoCipherUpdate", op_crypto_cipher_update),
        ("cryptoCipherFinal", op_crypto_cipher_final),
        ("cryptoCipherFinalGcm", op_crypto_cipher_final_gcm),
        ("cryptoCipherSetAad", op_crypto_cipher_set_aad),
        ("cryptoCipherGetAuthTag", op_crypto_cipher_get_auth_tag),
        ("cryptoCipherSetAuthTag", op_crypto_cipher_set_auth_tag),
        (
            "cryptoCipherSetAutoPadding",
            op_crypto_cipher_set_auto_padding
        ),
        // node:crypto wave 3: asymmetric sign/verify (RSA, ECDSA, Ed25519)
        ("cryptoSign", op_crypto_sign),
        ("cryptoVerify", op_crypto_verify),
        ("cryptoGenerateKeyPair", op_crypto_generate_keypair),
        // node:crypto wave 4: RSA encrypt/decrypt
        ("cryptoPublicEncrypt", op_crypto_public_encrypt),
        ("cryptoPrivateDecrypt", op_crypto_private_decrypt),
        // node:crypto wave 5: ECDH key agreement
        ("cryptoEcdhGenerateKeys", op_crypto_ecdh_generate_keys),
        ("cryptoEcdhComputeSecret", op_crypto_ecdh_compute_secret),
        ("cryptoEcdhGetPublicKey", op_crypto_ecdh_get_public_key),
        ("cryptoDhGenerateKeys", op_crypto_dh_generate_keys),
        ("cryptoDhComputeSecret", op_crypto_dh_compute_secret),
        // oam:permissions query surface
        ("permissionsQuery", op_permissions_query),
        // worker_threads
        ("workerNew", op_worker_new),
        ("workerPostMessage", op_worker_post_message),
        ("workerRecvMessage", op_worker_recv_message),
        ("workerTerminate", op_worker_terminate),
        ("parentPortPostMessage", op_parent_port_post_message),
        ("parentPortRecvMessage", op_parent_port_recv_message),
        ("workerThreadId", op_worker_thread_id),
        ("workerIsMainThread", op_worker_is_main_thread),
        ("workerGetData", op_worker_get_data),
        // child_process
        ("spawnSync", op_spawn_sync),
        ("spawnAsync", op_spawn_async),
        ("spawnKill", op_spawn_kill),
        ("spawnReadStdout", op_spawn_read_stdout),
        ("spawnReadStderr", op_spawn_read_stderr),
        ("spawnWrite", op_spawn_write),
        ("spawnWait", op_spawn_wait),
        // dns
        ("dnsLookup", op_dns_lookup),
        // stdin
        ("stdinRead", op_stdin_read),
        // os extended
        ("osRelease", op_os_release),
        ("osTotalMem", op_os_total_mem),
        ("osFreeMem", op_os_free_mem),
        // v8 heap / process memory
        ("heapStatistics", op_heap_statistics),
        ("processRss", op_process_rss),
        // cpu info
        ("cpuModel", op_cpu_model),
        ("cpuSpeed", op_cpu_speed),
        // network interfaces
        ("networkInterfaces", op_network_interfaces),
        // process.cpuUsage / process.kill
        ("processCpuUsage", op_process_cpu_usage),
        ("processKill", op_process_kill),
    );

    let node_key = v8::String::new(scope, "node").unwrap();
    internal.set(scope, node_key.into(), node.into());
}

// ----------------------------------------------------------------- helpers

pub(crate) fn throw_type_error(scope: &mut v8::PinScope<'_, '_>, message: &str) {
    let message = v8::String::new(scope, message).unwrap();
    let exception = v8::Exception::type_error(scope, message);
    scope.throw_exception(exception);
}

/// Throw an Error with Node's `.code` / `.syscall` / `.path` properties.
fn throw_node_error(
    scope: &mut v8::PinScope<'_, '_>,
    syscall: &str,
    path: &str,
    error: &std::io::Error,
) {
    let code = node_error_code(error);
    let message = node_error_message(code, syscall, path, error);
    let message_v8 =
        v8::String::new(scope, &message).unwrap_or_else(|| v8::String::new(scope, code).unwrap());
    let exception = v8::Exception::error(scope, message_v8);
    if let Ok(obj) = v8::Local::<v8::Object>::try_from(exception) {
        let props: [(&str, &str); 3] = [("code", code), ("syscall", syscall), ("path", path)];
        for (name, value) in props {
            let key = v8::String::new(scope, name).unwrap();
            if let Some(value) = v8::String::new(scope, value) {
                obj.set(scope, key.into(), value.into());
            }
        }
    }
    scope.throw_exception(exception);
}

pub(crate) fn arg_string(
    scope: &mut v8::PinScope<'_, '_>,
    args: &v8::FunctionCallbackArguments<'_>,
    index: i32,
) -> Option<String> {
    args.get(index)
        .to_string(scope)
        .map(|s| s.to_rust_string_lossy(scope))
}

/// Bytes from a write payload: ArrayBufferView copies, anything else goes
/// through ToString as UTF-8 (Node coerces the same way for strings).
pub(crate) fn arg_bytes(
    scope: &mut v8::PinScope<'_, '_>,
    args: &v8::FunctionCallbackArguments<'_>,
    index: i32,
) -> Option<Vec<u8>> {
    let value = args.get(index);
    if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(value) {
        let mut bytes = vec![0u8; view.byte_length()];
        let copied = view.copy_contents(&mut bytes);
        bytes.truncate(copied);
        return Some(bytes);
    }
    let text = value.to_string(scope)?.to_rust_string_lossy(scope);
    Some(text.into_bytes())
}

pub(crate) fn bytes_to_uint8array<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    bytes: Vec<u8>,
) -> Option<v8::Local<'s, v8::Value>> {
    let len = bytes.len();
    let store = v8::ArrayBuffer::new_backing_store_from_bytes(bytes.into_boxed_slice());
    let store = store.make_shared();
    let buffer = v8::ArrayBuffer::with_backing_store(scope, &store);
    let array = v8::Uint8Array::new(scope, buffer, 0, len)?;
    Some(array.into())
}

fn return_json(
    scope: &mut v8::PinScope<'_, '_>,
    rv: &mut v8::ReturnValue<'_, v8::Value>,
    json: &str,
) {
    if let Some(text) = v8::String::new(scope, json)
        && let Some(value) = v8::json::parse(scope, text)
    {
        rv.set(value);
    }
}

fn return_url_obj(
    scope: &mut v8::PinScope<'_, '_>,
    rv: &mut v8::ReturnValue<'_, v8::Value>,
    parsed: &ada_url::Url,
) {
    let obj = v8::Object::new(scope);
    let origin = parsed.origin();
    let props: [(&str, &str); 11] = [
        ("href", parsed.href()),
        ("protocol", parsed.protocol()),
        ("username", parsed.username()),
        ("password", parsed.password()),
        ("hostname", parsed.hostname()),
        ("port", parsed.port()),
        ("host", parsed.host()),
        ("pathname", parsed.pathname()),
        ("search", parsed.search()),
        ("hash", parsed.hash()),
        ("origin", &origin),
    ];
    for (key, val) in &props {
        if let (Some(k), Some(v)) = (v8::String::new(scope, key), v8::String::new(scope, val)) {
            obj.set(scope, k.into(), v.into());
        }
    }
    rv.set(obj.into());
}

// ----------------------------------------------------- process / os natives

fn op_env(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let env = v8::Object::new(scope);
    for (name, value) in std::env::vars() {
        let Some(key) = v8::String::new(scope, &name) else {
            continue;
        };
        let Some(value) = v8::String::new(scope, &value) else {
            continue;
        };
        env.set(scope, key.into(), value.into());
    }
    rv.set(env.into());
}

fn op_argv(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    // The embedder declares argv explicitly (JsRuntime::set_process_argv) —
    // re-deriving it from env::args() leaked oam's own flag VALUES into
    // script args and displaced argv[1]. No slot = embedded context: just
    // [exe], like a Node REPL.
    let argv: Vec<String> = scope
        .get_slot::<crate::ProcessArgv>()
        .map(|slot| slot.0.clone())
        .unwrap_or_else(|| {
            vec![
                std::env::current_exe()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| "oam".to_string()),
            ]
        });
    let elements: Vec<v8::Local<v8::Value>> = argv
        .iter()
        .filter_map(|a| v8::String::new(scope, a).map(Into::into))
        .collect();
    let array = v8::Array::new_with_elements(scope, &elements);
    rv.set(array.into());
}

fn op_cwd(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_string());
    if let Some(value) = v8::String::new(scope, &cwd) {
        rv.set(value.into());
    }
}

fn op_chdir(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(dir) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "chdir requires a path");
        return;
    };
    if let Err(e) = std::env::set_current_dir(&dir) {
        throw_node_error(scope, "chdir", &dir, &e);
    }
}

fn op_exit(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let code = args.get(0).int32_value(scope).unwrap_or(0);
    // Immediate exit, documented divergence (no 'exit' event flush).
    std::process::exit(code);
}

fn op_stdout_write(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    // arg_bytes: views pass through VERBATIM (binary pipes stay binary),
    // strings encode as UTF-8.
    if let Some(bytes) = arg_bytes(scope, &args, 0) {
        use std::io::Write;
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        let _ = lock.write_all(&bytes);
        let _ = lock.flush();
    }
}

fn op_stderr_write(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    if let Some(bytes) = arg_bytes(scope, &args, 0) {
        use std::io::Write;
        let stderr = std::io::stderr();
        let mut lock = stderr.lock();
        let _ = lock.write_all(&bytes);
        let _ = lock.flush();
    }
}

fn op_is_tty(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    use std::io::IsTerminal;
    let fd = args.get(0).int32_value(scope).unwrap_or(-1);
    let is_tty = match fd {
        0 => std::io::stdin().is_terminal(),
        1 => std::io::stdout().is_terminal(),
        2 => std::io::stderr().is_terminal(),
        _ => false,
    };
    rv.set_bool(is_tty);
}

fn op_now_ms(
    _scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let elapsed = START.get_or_init(Instant::now).elapsed();
    rv.set_double(elapsed.as_secs_f64() * 1000.0);
}

fn op_hrtime_nanos(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let elapsed = START.get_or_init(Instant::now).elapsed();
    let nanos = v8::BigInt::new_from_u64(scope, elapsed.as_nanos() as u64);
    rv.set(nanos.into());
}

fn op_uptime_ms(
    _scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let elapsed = START.get_or_init(Instant::now).elapsed();
    rv.set_double(elapsed.as_secs_f64() * 1000.0);
}

fn op_homedir(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    if let Some(value) = v8::String::new(scope, &home) {
        rv.set(value.into());
    }
}

fn op_tmpdir(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let tmp = std::env::temp_dir();
    // Node's tmpdir has no trailing separator.
    let text = tmp.to_string_lossy();
    let text = text.trim_end_matches(['/', '\\']);
    if let Some(value) = v8::String::new(scope, text) {
        rv.set(value.into());
    }
}

fn op_hostname(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let host = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "localhost".to_string());
    if let Some(value) = v8::String::new(scope, &host) {
        rv.set(value.into());
    }
}

fn op_username(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_default();
    if let Some(value) = v8::String::new(scope, &user) {
        rv.set(value.into());
    }
}

// ------------------------------------------------------------ http server

fn http_state(
    scope: &mut v8::PinScope<'_, '_>,
) -> std::sync::Arc<oam_core::http_server::HttpState> {
    scope
        .get_slot::<oam_core::CoreRuntime>()
        .expect("core runtime installed")
        .http()
}

/// Headers travel as a JSON [[name, value], ...] string both directions.
fn parse_headers_json(json: &str) -> Vec<(String, String)> {
    serde_json::from_str::<Vec<(String, String)>>(json).unwrap_or_default()
}

fn op_http_serve(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let host = arg_string(scope, &args, 0).unwrap_or_else(|| "127.0.0.1".to_string());
    let port = args.get(1).number_value(scope).unwrap_or(0.0) as u16;
    // Net gate: "host:port" is the resource being bound.
    let net_resource = format!("{host}:{port}");
    if !check_net_perm(scope, &net_resource) {
        return;
    }
    let state = http_state(scope);
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::http_server::http_serve(state, host, port),
    );
}

fn op_http_accept(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let server_id = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let state = http_state(scope);
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::http_server::http_accept(state, server_id),
    );
}

fn op_http_request_body(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let id = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let bytes = http_state(scope).take_request_body(id).unwrap_or_default();
    if let Some(value) = bytes_to_uint8array(scope, bytes) {
        rv.set(value);
    }
}

fn op_http_respond(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let id = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let status = args.get(1).number_value(scope).unwrap_or(200.0) as u16;
    let headers = arg_string(scope, &args, 2)
        .map(|j| parse_headers_json(&j))
        .unwrap_or_default();
    let body = arg_bytes(scope, &args, 3).unwrap_or_default();
    rv.set_bool(http_state(scope).respond_full(id, status, headers, body));
}

fn op_http_respond_stream(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let id = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let status = args.get(1).number_value(scope).unwrap_or(200.0) as u16;
    let headers = arg_string(scope, &args, 2)
        .map(|j| parse_headers_json(&j))
        .unwrap_or_default();
    match http_state(scope).respond_stream(id, status, headers) {
        Some(stream_id) => rv.set_double(stream_id as f64),
        None => throw_type_error(scope, "request already responded or connection gone"),
    }
}

fn op_http_body_push(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let stream_id = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let Some(bytes) = arg_bytes(scope, &args, 1) else {
        throw_type_error(scope, "httpBodyPush requires data");
        return;
    };
    let state = http_state(scope);
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::http_server::http_body_push(state, stream_id, bytes),
    );
}

fn op_http_body_end(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let stream_id = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    http_state(scope).end_stream(stream_id);
}

fn op_http_close(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let server_id = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    http_state(scope).close_server(server_id);
}

// ------------------------------------------------------------------- TCP

fn op_tcp_connect(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(host) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "tcpConnect requires a host");
        return;
    };
    let port = args.get(1).number_value(scope).unwrap_or(0.0) as u16;
    let net_resource = format!("{host}:{port}");
    if !check_net_perm(scope, &net_resource) {
        return;
    }
    let core = scope
        .get_slot::<oam_core::CoreRuntime>()
        .expect("core runtime installed");
    let tcp = core.tcp();
    let ids = core.body_ids();
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::tcp::tcp_connect(tcp, ids, host, port),
    );
}

fn op_tcp_read(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let len = args.get(1).number_value(scope).unwrap_or(65536.0) as usize;
    let tcp = scope
        .get_slot::<oam_core::CoreRuntime>()
        .expect("core runtime installed")
        .tcp();
    crate::ops::spawn_op(scope, &mut rv, oam_core::tcp::tcp_read(tcp, handle, len));
}

fn op_tcp_write(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let Some(data) = arg_bytes(scope, &args, 1) else {
        throw_type_error(scope, "tcpWrite requires data");
        return;
    };
    let tcp = scope
        .get_slot::<oam_core::CoreRuntime>()
        .expect("core runtime installed")
        .tcp();
    crate::ops::spawn_op(scope, &mut rv, oam_core::tcp::tcp_write(tcp, handle, data));
}

fn op_tcp_close(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let tcp = scope
        .get_slot::<oam_core::CoreRuntime>()
        .expect("core runtime installed")
        .tcp();
    oam_core::tcp::tcp_close(&tcp, handle);
}

fn op_tcp_shutdown(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let tcp = scope
        .get_slot::<oam_core::CoreRuntime>()
        .expect("core runtime installed")
        .tcp();
    crate::ops::spawn_op(scope, &mut rv, oam_core::tcp::tcp_shutdown(tcp, handle));
}

fn op_tcp_listen(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(host) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "tcpListen requires a host");
        return;
    };
    let port = args.get(1).number_value(scope).unwrap_or(0.0) as u16;
    let net_resource = format!("{host}:{port}");
    if !check_net_perm(scope, &net_resource) {
        return;
    }
    let core = scope
        .get_slot::<oam_core::CoreRuntime>()
        .expect("core runtime installed");
    let tcp = core.tcp();
    let ids = core.body_ids();
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::tcp::tcp_listen(tcp, ids, host, port),
    );
}

fn op_tcp_accept(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let server_id = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let core = scope
        .get_slot::<oam_core::CoreRuntime>()
        .expect("core runtime installed");
    let tcp = core.tcp();
    let ids = core.body_ids();
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::tcp::tcp_accept(tcp, server_id, ids),
    );
}

fn op_tcp_server_close(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let server_id = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let tcp = scope
        .get_slot::<oam_core::CoreRuntime>()
        .expect("core runtime installed")
        .tcp();
    oam_core::tcp::tcp_server_close(&tcp, server_id);
}

/// zlibSync(bytes, format, level, compress) — synchronous transform on the
/// isolate thread (the *Sync API contract). "unzip" auto-detects on decode.
fn op_zlib_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(bytes) = arg_bytes(scope, &args, 0) else {
        throw_type_error(scope, "zlib requires data");
        return;
    };
    let format = arg_string(scope, &args, 1).unwrap_or_default();
    let level = args.get(2).int32_value(scope).unwrap_or(-1);
    let compress = args.get(3).is_true();
    let result = if !compress && format == "unzip" {
        oam_core::zlib::unzip(&bytes)
    } else {
        match oam_core::zlib::Format::parse(&format) {
            Some(parsed) => {
                if compress {
                    oam_core::zlib::compress(&bytes, parsed, level)
                } else {
                    oam_core::zlib::decompress(&bytes, parsed)
                }
            }
            None => {
                throw_type_error(scope, &format!("unknown zlib format '{format}'"));
                return;
            }
        }
    };
    match result {
        Ok(out) => {
            if let Some(value) = bytes_to_uint8array(scope, out) {
                rv.set(value);
            }
        }
        Err(e) => {
            let message = v8::String::new(scope, &format!("zlib: {e}")).unwrap();
            let exception = v8::Exception::error(scope, message);
            scope.throw_exception(exception);
        }
    }
}

/// zlibAsync(bytes, format, level, compress) -> Promise<Uint8Array>.
fn op_zlib_async(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(bytes) = arg_bytes(scope, &args, 0) else {
        throw_type_error(scope, "zlib requires data");
        return;
    };
    let format = arg_string(scope, &args, 1).unwrap_or_default();
    let level = args.get(2).int32_value(scope).unwrap_or(-1);
    let compress = args.get(3).is_true();
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::ops::zlib_transform(bytes, format, level, compress),
    );
}

/// zlibStreamCreate(format, level, compress) -> Promise<{handle}>.
/// Allocates an incremental compressor or decompressor in the stream
/// registry. The handle is passed to subsequent write/flush/close calls.
fn op_zlib_stream_create(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let format = arg_string(scope, &args, 0).unwrap_or_default();
    let level = args.get(1).int32_value(scope).unwrap_or(-1);
    let compress = args.get(2).is_true();
    let core = scope
        .get_slot::<oam_core::CoreRuntime>()
        .expect("core runtime installed");
    let streams = core.zlib_streams();
    let ids = core.body_ids();
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::ops::zlib_stream_create(streams, ids, format, level, compress),
    );
}

/// zlibStreamWrite(handle, chunk) -> Promise<Uint8Array>.
/// Feed one chunk into an existing stream. Resolves with bytes produced
/// immediately (may be empty if the encoder is still filling a block).
fn op_zlib_stream_write(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let Some(chunk) = arg_bytes(scope, &args, 1) else {
        throw_type_error(scope, "zlibStreamWrite requires data");
        return;
    };
    let streams = scope
        .get_slot::<oam_core::CoreRuntime>()
        .expect("core runtime installed")
        .zlib_streams();
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::ops::zlib_stream_write(streams, handle, chunk),
    );
}

/// zlibStreamFlush(handle) -> Promise<Uint8Array>.
/// Finalize the stream and return tail bytes. The stream handle is
/// removed from the registry after this call.
fn op_zlib_stream_flush(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let streams = scope
        .get_slot::<oam_core::CoreRuntime>()
        .expect("core runtime installed")
        .zlib_streams();
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::ops::zlib_stream_flush(streams, handle),
    );
}

/// zlibStreamClose(handle) -> void (synchronous).
/// Discard the stream without flushing. Used when the stream is destroyed
/// before it completes normally.
fn op_zlib_stream_close(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let streams = scope
        .get_slot::<oam_core::CoreRuntime>()
        .expect("core runtime installed")
        .zlib_streams();
    oam_core::ops::zlib_stream_close(&streams, handle);
}

fn op_url_parse_href(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(input) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "Invalid URL");
        return;
    };
    let base = args.get(1);
    let base = if base.is_null_or_undefined() {
        None
    } else {
        match base.to_string(scope).map(|s| s.to_rust_string_lossy(scope)) {
            Some(base) => Some(base),
            None => {
                throw_type_error(scope, "Invalid base URL");
                return;
            }
        }
    };
    match ada_url::Url::parse(&input, base.as_deref()) {
        Ok(parsed) => {
            if let Some(s) = v8::String::new(scope, parsed.href()) {
                rv.set(s.into());
            }
        }
        Err(_) => throw_type_error(scope, &format!("Invalid URL: {input}")),
    }
}

fn op_url_can_parse(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(input) = arg_string(scope, &args, 0) else {
        rv.set(v8::Boolean::new(scope, false).into());
        return;
    };
    let base = args.get(1);
    let base = if base.is_null_or_undefined() {
        None
    } else {
        match base.to_string(scope).map(|s| s.to_rust_string_lossy(scope)) {
            Some(base) => Some(base),
            None => {
                rv.set(v8::Boolean::new(scope, false).into());
                return;
            }
        }
    };
    let ok = ada_url::Url::can_parse(&input, base.as_deref());
    rv.set(v8::Boolean::new(scope, ok).into());
}

fn op_url_parse(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(input) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "Invalid URL");
        return;
    };
    let base = args.get(1);
    let base = if base.is_null_or_undefined() {
        None
    } else {
        match base.to_string(scope).map(|s| s.to_rust_string_lossy(scope)) {
            Some(base) => Some(base),
            None => {
                throw_type_error(scope, "Invalid base URL");
                return;
            }
        }
    };
    match ada_url::Url::parse(&input, base.as_deref()) {
        Ok(parsed) => return_url_obj(scope, &mut rv, &parsed),
        Err(_) => throw_type_error(scope, &format!("Invalid URL: {input}")),
    }
}

fn op_url_update(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let (Some(href), Some(part), Some(value)) = (
        arg_string(scope, &args, 0),
        arg_string(scope, &args, 1),
        arg_string(scope, &args, 2),
    ) else {
        throw_type_error(scope, "urlUpdate requires href, part, value");
        return;
    };
    match update_url(&href, &part, &value) {
        Ok(parsed) => return_url_obj(scope, &mut rv, &parsed),
        Err(msg) => throw_type_error(scope, &msg),
    }
}

fn update_url(href: &str, part: &str, value: &str) -> Result<ada_url::Url, String> {
    let mut parsed = ada_url::Url::parse(href, None).map_err(|_| format!("Invalid URL: {href}"))?;
    match part {
        "__noop" => {}
        "protocol" => {
            let _ = parsed.set_protocol(value);
        }
        "username" => {
            let _ = parsed.set_username(Some(value));
        }
        "password" => {
            let _ = parsed.set_password(if value.is_empty() { None } else { Some(value) });
        }
        "host" => {
            let _ = parsed.set_host(Some(value));
        }
        "hostname" => {
            let _ = parsed.set_hostname(Some(value));
        }
        "port" => {
            let _ = parsed.set_port(if value.is_empty() { None } else { Some(value) });
        }
        "pathname" => {
            let _ = parsed.set_pathname(Some(value));
        }
        "search" => {
            if value.is_empty() {
                parsed.set_search(None);
            } else {
                parsed.set_search(Some(value));
            }
        }
        "hash" => {
            if value.is_empty() {
                parsed.set_hash(None);
            } else {
                parsed.set_hash(Some(value));
            }
        }
        other => return Err(format!("urlUpdate: unknown part '{other}'")),
    }
    Ok(parsed)
}

/// Read the current continuation frame (an immutable Map of
/// AsyncLocalStorage -> store, or undefined). V8 snapshots this value into
/// every promise reaction at creation and restores it when the reaction
/// runs — which is exactly AsyncLocalStorage's await semantics, for free.
fn op_get_continuation_data(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    rv.set(scope.get_continuation_preserved_embedder_data());
}

fn op_set_continuation_data(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    scope.set_continuation_preserved_embedder_data(args.get(0));
}

fn op_make_require(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(filename) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "createRequire requires a filename or file URL");
        return;
    };
    if let Some(require) = crate::cjs::make_require(scope, &filename) {
        rv.set(require.into());
    }
}

// ------------------------------------------------------------ fs sync ops

fn op_fs_read_file_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "readFileSync requires a path");
        return;
    };
    if !check_read_perm(scope, &path) {
        return;
    }
    // Always raw bytes: encodings decode JS-side via Buffer#toString, so
    // 'base64'/'hex'/'latin1' behave instead of utf8-lossy garbage.
    match std::fs::read(&path) {
        Ok(bytes) => {
            if let Some(value) = bytes_to_uint8array(scope, bytes) {
                rv.set(value);
            }
        }
        Err(e) => throw_node_error(scope, "open", &path, &e),
    }
}

fn op_fs_read_file_utf8_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "readFileSync requires a path");
        return;
    };
    if !check_read_perm(scope, &path) {
        return;
    }
    match std::fs::read(&path) {
        Ok(bytes) => {
            if let Some(s) = v8::String::new_from_utf8(scope, &bytes, v8::NewStringType::Normal) {
                rv.set(s.into());
            }
        }
        Err(e) => throw_node_error(scope, "open", &path, &e),
    }
}

fn op_fs_write_file_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "writeFileSync requires a path");
        return;
    };
    if !check_write_perm(scope, &path) {
        return;
    }
    let Some(bytes) = arg_bytes(scope, &args, 1) else {
        throw_type_error(scope, "writeFileSync requires data");
        return;
    };
    let append = args.get(2).is_true();
    let result = if append {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| f.write_all(&bytes))
    } else {
        std::fs::write(&path, &bytes)
    };
    if let Err(e) = result {
        throw_node_error(scope, "open", &path, &e);
    }
}

fn op_fs_exists_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        rv.set_bool(false);
        return;
    };
    rv.set_bool(std::path::Path::new(&path).exists());
}

fn op_fs_stat_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "statSync requires a path");
        return;
    };
    if !check_read_perm(scope, &path) {
        return;
    }
    let lstat = args.get(1).is_true();
    let meta = if lstat {
        std::fs::symlink_metadata(&path)
    } else {
        std::fs::metadata(&path)
    };
    match meta {
        Ok(meta) => return_json(scope, &mut rv, &oam_core::ops::stat_to_json(&meta)),
        Err(e) => throw_node_error(scope, if lstat { "lstat" } else { "stat" }, &path, &e),
    }
}

fn op_fs_readdir_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "readdirSync requires a path");
        return;
    };
    if !check_read_perm(scope, &path) {
        return;
    }
    match oam_core::ops::readdir_to_json(&path) {
        Ok(json) => return_json(scope, &mut rv, &json),
        Err(e) => throw_node_error(scope, "scandir", &path, &e),
    }
}

fn op_fs_mkdir_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "mkdirSync requires a path");
        return;
    };
    let recursive = args.get(1).is_true();
    let result = if recursive {
        std::fs::create_dir_all(&path)
    } else {
        std::fs::create_dir(&path)
    };
    if let Err(e) = result {
        throw_node_error(scope, "mkdir", &path, &e);
    }
}

fn op_fs_rm_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "rmSync requires a path");
        return;
    };
    let recursive = args.get(1).is_true();
    let force = args.get(2).is_true();
    match oam_core::remove_path(&path, recursive) {
        Ok(()) => {}
        Err(e) if force && e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => throw_node_error(scope, "rm", &path, &e),
    }
}

fn op_fs_rename_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let (Some(from), Some(to)) = (arg_string(scope, &args, 0), arg_string(scope, &args, 1)) else {
        throw_type_error(scope, "renameSync requires from and to paths");
        return;
    };
    if let Err(e) = std::fs::rename(&from, &to) {
        throw_node_error(scope, "rename", &from, &e);
    }
}

fn op_fs_copy_file_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let (Some(from), Some(to)) = (arg_string(scope, &args, 0), arg_string(scope, &args, 1)) else {
        throw_type_error(scope, "copyFileSync requires from and to paths");
        return;
    };
    if let Err(e) = std::fs::copy(&from, &to) {
        throw_node_error(scope, "copyfile", &from, &e);
    }
}

fn op_fs_unlink_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "unlinkSync requires a path");
        return;
    };
    if let Err(e) = std::fs::remove_file(&path) {
        throw_node_error(scope, "unlink", &path, &e);
    }
}

fn op_fs_access_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "accessSync requires a path");
        return;
    };
    let mode = args.get(1).int32_value(scope).unwrap_or(0);
    match oam_core::check_access(&path, mode) {
        Ok(()) => {}
        Err((code, message)) => {
            // EPERM/EACCES with the path attached, same shape as
            // throw_node_error but with the access-specific code.
            let message_v8 = v8::String::new(scope, &message)
                .unwrap_or_else(|| v8::String::new(scope, &code).unwrap());
            let exception = v8::Exception::error(scope, message_v8);
            if let Ok(obj) = v8::Local::<v8::Object>::try_from(exception) {
                let props: [(&str, &str); 3] =
                    [("code", &code), ("syscall", "access"), ("path", &path)];
                for (name, value) in props {
                    let key = v8::String::new(scope, name).unwrap();
                    if let Some(value) = v8::String::new(scope, value) {
                        obj.set(scope, key.into(), value.into());
                    }
                }
            }
            scope.throw_exception(exception);
        }
    }
}

fn op_fs_realpath_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "realpathSync requires a path");
        return;
    };
    match std::fs::canonicalize(PathBuf::from(&path)) {
        Ok(real) => {
            let text = oam_core::strip_unc_prefix(&real);
            if let Some(value) = v8::String::new(scope, &text) {
                rv.set(value.into());
            }
        }
        Err(e) => throw_node_error(scope, "realpath", &path, &e),
    }
}

// ----------------------------------------------------------- fs async ops

fn op_fs_read_file(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "readFile requires a path");
        return;
    };
    if !check_read_perm(scope, &path) {
        return;
    }
    // Always raw bytes; encodings decode JS-side (see the sync twin).
    crate::ops::spawn_op(scope, &mut rv, oam_core::ops::fs_read_file(path));
}

fn op_fs_write_file(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "writeFile requires a path");
        return;
    };
    if !check_write_perm(scope, &path) {
        return;
    }
    let Some(bytes) = arg_bytes(scope, &args, 1) else {
        throw_type_error(scope, "writeFile requires data");
        return;
    };
    let append = args.get(2).is_true();
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::ops::fs_write_file(path, bytes, append),
    );
}

fn op_fs_stat(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "stat requires a path");
        return;
    };
    let lstat = args.get(1).is_true();
    crate::ops::spawn_op(scope, &mut rv, oam_core::ops::fs_stat(path, lstat));
}

fn op_fs_readdir(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "readdir requires a path");
        return;
    };
    crate::ops::spawn_op(scope, &mut rv, oam_core::ops::fs_readdir(path));
}

fn op_fs_mkdir(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "mkdir requires a path");
        return;
    };
    let recursive = args.get(1).is_true();
    crate::ops::spawn_op(scope, &mut rv, oam_core::ops::fs_mkdir(path, recursive));
}

fn op_fs_rm(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "rm requires a path");
        return;
    };
    let recursive = args.get(1).is_true();
    let force = args.get(2).is_true();
    crate::ops::spawn_op(scope, &mut rv, oam_core::ops::fs_rm(path, recursive, force));
}

fn op_fs_rename(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let (Some(from), Some(to)) = (arg_string(scope, &args, 0), arg_string(scope, &args, 1)) else {
        throw_type_error(scope, "rename requires from and to paths");
        return;
    };
    crate::ops::spawn_op(scope, &mut rv, oam_core::ops::fs_rename(from, to));
}

fn op_fs_copy_file(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let (Some(from), Some(to)) = (arg_string(scope, &args, 0), arg_string(scope, &args, 1)) else {
        throw_type_error(scope, "copyFile requires from and to paths");
        return;
    };
    crate::ops::spawn_op(scope, &mut rv, oam_core::ops::fs_copy_file(from, to));
}

fn op_fs_unlink(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "unlink requires a path");
        return;
    };
    crate::ops::spawn_op(scope, &mut rv, oam_core::ops::fs_unlink(path));
}

fn op_fs_access(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "access requires a path");
        return;
    };
    let mode = args.get(1).int32_value(scope).unwrap_or(0);
    crate::ops::spawn_op(scope, &mut rv, oam_core::ops::fs_access(path, mode));
}

fn op_fs_realpath(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "realpath requires a path");
        return;
    };
    crate::ops::spawn_op(scope, &mut rv, oam_core::ops::fs_realpath(path));
}

fn op_fs_mkdtemp(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let prefix = arg_string(scope, &args, 0).unwrap_or_default();
    crate::ops::spawn_op(scope, &mut rv, oam_core::ops::fs_mkdtemp(prefix));
}

fn op_fs_symlink(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(target) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "symlink requires a target");
        return;
    };
    let Some(path) = arg_string(scope, &args, 1) else {
        throw_type_error(scope, "symlink requires a path");
        return;
    };
    crate::ops::spawn_op(scope, &mut rv, oam_core::ops::fs_symlink(target, path));
}

fn op_fs_readlink(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "readlink requires a path");
        return;
    };
    crate::ops::spawn_op(scope, &mut rv, oam_core::ops::fs_readlink(path));
}

fn op_fs_link(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(existing) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "link requires an existing path");
        return;
    };
    let Some(new_path) = arg_string(scope, &args, 1) else {
        throw_type_error(scope, "link requires a new path");
        return;
    };
    crate::ops::spawn_op(scope, &mut rv, oam_core::ops::fs_link(existing, new_path));
}

fn op_fs_chmod(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "chmod requires a path");
        return;
    };
    let mode = args.get(1).uint32_value(scope).unwrap_or(0o644);
    crate::ops::spawn_op(scope, &mut rv, oam_core::ops::fs_chmod(path, mode));
}

fn op_fs_truncate(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "truncate requires a path");
        return;
    };
    let len = args.get(1).integer_value(scope).unwrap_or(0).max(0) as u64;
    crate::ops::spawn_op(scope, &mut rv, oam_core::ops::fs_truncate(path, len));
}

fn op_fs_symlink_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(target) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "symlinkSync requires a target");
        return;
    };
    let Some(path) = arg_string(scope, &args, 1) else {
        throw_type_error(scope, "symlinkSync requires a path");
        return;
    };
    #[cfg(windows)]
    let result = {
        let is_dir = std::fs::metadata(&target).map(|m| m.is_dir()).unwrap_or(false);
        if is_dir {
            std::os::windows::fs::symlink_dir(&target, &path)
        } else {
            std::os::windows::fs::symlink_file(&target, &path)
        }
    };
    #[cfg(not(windows))]
    let result = std::os::unix::fs::symlink(&target, &path);
    if let Err(e) = result {
        throw_node_error(scope, "symlink", &path, &e);
    }
}

fn op_fs_readlink_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "readlinkSync requires a path");
        return;
    };
    match std::fs::read_link(&path) {
        Ok(target) => {
            let text = oam_core::strip_unc_prefix(&target);
            if let Some(value) = v8::String::new(scope, &text) {
                rv.set(value.into());
            }
        }
        Err(e) => throw_node_error(scope, "readlink", &path, &e),
    }
}

fn op_fs_link_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(existing) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "linkSync requires an existing path");
        return;
    };
    let Some(new_path) = arg_string(scope, &args, 1) else {
        throw_type_error(scope, "linkSync requires a new path");
        return;
    };
    if let Err(e) = std::fs::hard_link(&existing, &new_path) {
        throw_node_error(scope, "link", &new_path, &e);
    }
}

fn op_fs_chmod_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "chmodSync requires a path");
        return;
    };
    let mode = args.get(1).uint32_value(scope).unwrap_or(0o644);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)) {
            throw_node_error(scope, "chmod", &path, &e);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
        match std::fs::metadata(&path) {
            Ok(meta) => {
                let mut perms = meta.permissions();
                perms.set_readonly(mode & 0o200 == 0);
                if let Err(e) = std::fs::set_permissions(&path, perms) {
                    throw_node_error(scope, "chmod", &path, &e);
                }
            }
            Err(e) => throw_node_error(scope, "chmod", &path, &e),
        }
    }
}

fn op_fs_truncate_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "truncateSync requires a path");
        return;
    };
    let len = args.get(1).integer_value(scope).unwrap_or(0).max(0) as u64;
    match std::fs::OpenOptions::new().write(true).open(&path) {
        Ok(f) => {
            if let Err(e) = f.set_len(len) {
                throw_node_error(scope, "truncate", &path, &e);
            }
        }
        Err(e) => throw_node_error(scope, "truncate", &path, &e),
    }
}

fn op_fs_mkdtemp_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let prefix = arg_string(scope, &args, 0).unwrap_or_default();
    let dir = std::env::temp_dir().join(format!(
        "{}{}",
        prefix,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    match std::fs::create_dir(&dir) {
        Ok(()) => {
            let text = oam_core::strip_unc_prefix(&dir);
            if let Some(value) = v8::String::new(scope, &text) {
                rv.set(value.into());
            }
        }
        Err(e) => throw_node_error(scope, "mkdtemp", &prefix, &e),
    }
}

// ---------------------------------------------------------- fs stream ops

fn op_fs_open(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "fsOpen requires a path");
        return;
    };
    let mode = arg_string(scope, &args, 1).unwrap_or_else(|| "r".to_string());
    // Gate on read for read modes ("r", "r+"), write for write modes.
    let is_write = mode.contains('w') || mode.contains('a');
    if is_write {
        if !check_write_perm(scope, &path) {
            return;
        }
    } else if !check_read_perm(scope, &path) {
        return;
    }
    let core = scope
        .get_slot::<oam_core::CoreRuntime>()
        .expect("core runtime installed");
    let files = core.files();
    let ids = core.body_ids();
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::ops::fs_open(files, ids, path, mode),
    );
}

fn op_fs_read_chunk(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let len = args.get(1).number_value(scope).unwrap_or(65536.0) as usize;
    let files = scope
        .get_slot::<oam_core::CoreRuntime>()
        .expect("core runtime installed")
        .files();
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::ops::fs_read_chunk(files, handle, len),
    );
}

fn op_fs_write_chunk(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let Some(bytes) = arg_bytes(scope, &args, 1) else {
        throw_type_error(scope, "fsWriteChunk requires data");
        return;
    };
    let files = scope
        .get_slot::<oam_core::CoreRuntime>()
        .expect("core runtime installed")
        .files();
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::ops::fs_write_chunk(files, handle, bytes),
    );
}

/// Synchronous: dropping the File closes it (flush happens in write ops).
fn op_fs_close(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let files = scope
        .get_slot::<oam_core::CoreRuntime>()
        .expect("core runtime installed")
        .files();
    let mut guard = files.lock().expect("file registry lock");
    // If the File is in flight (removed by a chunk op for its IO await),
    // it is absent here -- record the close so the op's reinsert drops it
    // instead of resurrecting a leaked fd (destroy()-during-read race).
    if guard.files.remove(&handle).is_none() {
        guard.closed.insert(handle);
    }
}

// ------------------------------------------------- permission helpers / query

/// Throw an `Error` with `.code = "ERR_PERMISSION_DENIED"` and return.
/// The message is the full Deno-shaped description from `Permissions`.
fn throw_permission_denied(scope: &mut v8::PinScope<'_, '_>, message: &str) {
    let msg_v8 = v8::String::new(scope, message)
        .unwrap_or_else(|| v8::String::new(scope, "ERR_PERMISSION_DENIED").unwrap());
    let exception = v8::Exception::error(scope, msg_v8);
    if let Ok(obj) = v8::Local::<v8::Object>::try_from(exception) {
        let key = v8::String::new(scope, "code").unwrap();
        if let Some(code) = v8::String::new(scope, "ERR_PERMISSION_DENIED") {
            obj.set(scope, key.into(), code.into());
        }
    }
    scope.throw_exception(exception);
}

/// Get a clone of the current `Permissions` slot, or an all-granted default.
/// Clone is cheap (three enum variants, no heap except for List paths).
fn get_permissions(scope: &v8::PinScope<'_, '_>) -> crate::permissions::Permissions {
    scope
        .get_slot::<crate::permissions::Permissions>()
        .cloned()
        .unwrap_or_default()
}

/// Check `read` permission for `path`; throw and return `false` if denied.
/// Usage:
/// ```ignore
/// if !check_read_perm(scope, &path) { return; }
/// ```
fn check_read_perm(scope: &mut v8::PinScope<'_, '_>, path: &str) -> bool {
    if let Err(msg) = get_permissions(scope).check_read(path) {
        throw_permission_denied(scope, &msg);
        false
    } else {
        true
    }
}

/// Check `write` permission for `path`.
fn check_write_perm(scope: &mut v8::PinScope<'_, '_>, path: &str) -> bool {
    if let Err(msg) = get_permissions(scope).check_write(path) {
        throw_permission_denied(scope, &msg);
        false
    } else {
        true
    }
}

/// Check `net` permission for `host`.
fn check_net_perm(scope: &mut v8::PinScope<'_, '_>, host: &str) -> bool {
    if let Err(msg) = get_permissions(scope).check_net(host) {
        throw_permission_denied(scope, &msg);
        false
    } else {
        true
    }
}

/// permissionsQuery(name: string, target: string | null) -> {state: string}
///
/// Called by js/permissions.js to implement `permissions.query()`.
fn op_permissions_query(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let name = arg_string(scope, &args, 0).unwrap_or_default();
    // Second arg is null (no target) or a string path/host.
    let target = if args.get(1).is_null_or_undefined() {
        None
    } else {
        arg_string(scope, &args, 1)
    };
    let state = get_permissions(scope).query_state(&name, target.as_deref());
    let result = v8::Object::new(scope);
    let state_key = v8::String::new(scope, "state").unwrap();
    let state_val = v8::String::new(scope, state).unwrap();
    result.set(scope, state_key.into(), state_val.into());
    rv.set(result.into());
}

// -------------------------------------------------------------- worker_threads

fn op_worker_new(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(script_path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "workerNew requires a script path");
        return;
    };
    let worker_data = if args.get(1).is_null_or_undefined() {
        None
    } else {
        arg_string(scope, &args, 1)
    };

    let path = PathBuf::from(&script_path);
    if !path.is_file() {
        throw_type_error(scope, &format!("worker script not found: {script_path}"));
        return;
    }

    let core = scope
        .get_slot::<oam_core::CoreRuntime>()
        .expect("core runtime installed");
    let workers = core.workers();

    let worker_id = workers.lock().expect("worker registry lock").next_id();

    let (parent_to_worker_tx, parent_to_worker_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let (worker_to_parent_tx, worker_to_parent_rx) =
        std::sync::mpsc::channel::<oam_core::worker::WorkerEvent>();

    let thread = crate::worker::spawn_worker(
        path,
        worker_data,
        worker_id,
        parent_to_worker_rx,
        worker_to_parent_tx,
    );

    {
        let mut guard = workers.lock().expect("worker registry lock");
        guard.handles.insert(
            worker_id,
            oam_core::worker::WorkerHandle {
                to_worker: parent_to_worker_tx,
                thread: Some(thread),
            },
        );
        guard.receivers.insert(worker_id, worker_to_parent_rx);
    }

    let result = v8::Object::new(scope);
    let id_key = v8::String::new(scope, "workerId").unwrap();
    let id_val = v8::Number::new(scope, worker_id as f64);
    result.set(scope, id_key.into(), id_val.into());
    let tid_key = v8::String::new(scope, "threadId").unwrap();
    let tid_val = v8::Number::new(scope, worker_id as f64);
    result.set(scope, tid_key.into(), tid_val.into());
    rv.set(result.into());
}

fn op_worker_post_message(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let worker_id = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let Some(json) = arg_string(scope, &args, 1) else {
        throw_type_error(scope, "workerPostMessage requires data");
        return;
    };
    let workers = scope
        .get_slot::<oam_core::CoreRuntime>()
        .expect("core runtime installed")
        .workers();
    if let Err(msg) = oam_core::worker::parent_post(&workers, worker_id, json.into_bytes()) {
        let msg_v8 = v8::String::new(scope, &msg).unwrap();
        let exception = v8::Exception::error(scope, msg_v8);
        scope.throw_exception(exception);
    }
}

fn op_worker_recv_message(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let worker_id = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let workers = scope
        .get_slot::<oam_core::CoreRuntime>()
        .expect("core runtime installed")
        .workers();
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::worker::parent_recv(workers, worker_id),
    );
}

fn op_worker_terminate(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let worker_id = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let workers = scope
        .get_slot::<oam_core::CoreRuntime>()
        .expect("core runtime installed")
        .workers();
    oam_core::worker::parent_terminate(&workers, worker_id);
}

fn op_parent_port_post_message(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(json) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "parentPortPostMessage requires data");
        return;
    };
    let ctx = scope.get_slot::<oam_core::worker::WorkerContext>();
    let Some(ctx) = ctx else {
        throw_type_error(scope, "not in a worker thread");
        return;
    };
    let outbox = ctx.outbox.clone();
    if let Err(msg) = oam_core::worker::worker_post(&outbox, json.into_bytes()) {
        let msg_v8 = v8::String::new(scope, &msg).unwrap();
        let exception = v8::Exception::error(scope, msg_v8);
        scope.throw_exception(exception);
    }
}

fn op_parent_port_recv_message(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let ctx = scope.get_slot::<oam_core::worker::WorkerContext>();
    let Some(ctx) = ctx else {
        throw_type_error(scope, "not in a worker thread");
        return;
    };
    let inbox = ctx.inbox.clone();
    crate::ops::spawn_op(scope, &mut rv, oam_core::worker::worker_recv(inbox));
}

fn op_worker_thread_id(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let thread_id = scope
        .get_slot::<oam_core::worker::WorkerContext>()
        .map(|ctx| ctx.thread_id)
        .unwrap_or(0);
    rv.set(v8::Number::new(scope, thread_id as f64).into());
}

fn op_worker_is_main_thread(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let is_main = scope
        .get_slot::<oam_core::worker::WorkerContext>()
        .is_none();
    rv.set(v8::Boolean::new(scope, is_main).into());
}

fn op_worker_get_data(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let data = scope
        .get_slot::<oam_core::worker::WorkerContext>()
        .and_then(|ctx| ctx.worker_data.clone());
    match data {
        Some(json) => {
            let parsed = v8::String::new(scope, &json).and_then(|s| v8::json::parse(scope, s));
            match parsed {
                Some(value) => rv.set(value),
                None => rv.set(v8::null(scope).into()),
            }
        }
        None => rv.set(v8::null(scope).into()),
    }
}

// -------------------------------------------------------- child_process ops

fn op_spawn_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(command) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "spawnSync: command required");
        return;
    };

    let args_val = args.get(1);
    let mut child_args: Vec<String> = Vec::new();
    if let Ok(arr) = v8::Local::<v8::Array>::try_from(args_val) {
        for i in 0..arr.length() {
            if let Some(v) = arr.get_index(scope, i) {
                if let Some(s) = v.to_string(scope) {
                    child_args.push(s.to_rust_string_lossy(scope));
                }
            }
        }
    }

    let opts_val = args.get(2);
    let opts = if opts_val.is_null_or_undefined() {
        None
    } else {
        v8::Local::<v8::Object>::try_from(opts_val).ok()
    };

    let cwd = opts
        .and_then(|o| {
            let key = v8::String::new(scope, "cwd")?;
            let val = o.get(scope, key.into())?;
            if val.is_null_or_undefined() {
                return None;
            }
            val.to_string(scope).map(|s| s.to_rust_string_lossy(scope))
        });
    let shell = opts
        .and_then(|o| {
            let key = v8::String::new(scope, "shell")?;
            o.get(scope, key.into())
        })
        .is_some_and(|v| v.is_true());
    let clear_env = opts
        .and_then(|o| {
            let key = v8::String::new(scope, "clearEnv")?;
            o.get(scope, key.into())
        })
        .is_some_and(|v| v.is_true());
    let timeout_ms = opts
        .and_then(|o| {
            let key = v8::String::new(scope, "timeout")?;
            let val = o.get(scope, key.into())?;
            val.number_value(scope)
        })
        .unwrap_or(0.0) as u64;
    let max_buffer = opts
        .and_then(|o| {
            let key = v8::String::new(scope, "maxBuffer")?;
            let val = o.get(scope, key.into())?;
            val.number_value(scope)
        })
        .map(|n| n as usize)
        .unwrap_or(50 * 1024 * 1024);

    let input = opts.and_then(|o| {
        let key = v8::String::new(scope, "input")?;
        let val = o.get(scope, key.into())?;
        if val.is_null_or_undefined() {
            return None;
        }
        if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(val) {
            let mut bytes = vec![0u8; view.byte_length()];
            let copied = view.copy_contents(&mut bytes);
            bytes.truncate(copied);
            return Some(bytes);
        }
        let text = val.to_string(scope)?.to_rust_string_lossy(scope);
        Some(text.into_bytes())
    });

    let env_pairs = opts.and_then(|o| {
        let key = v8::String::new(scope, "env")?;
        let val = o.get(scope, key.into())?;
        if val.is_null_or_undefined() {
            return None;
        }
        let env_obj = v8::Local::<v8::Object>::try_from(val).ok()?;
        let names = env_obj.get_own_property_names(scope, Default::default())?;
        let mut pairs = Vec::new();
        for i in 0..names.length() {
            if let Some(name) = names.get_index(scope, i) {
                if let Some(val) = env_obj.get(scope, name) {
                    if let (Some(k), Some(v)) = (
                        name.to_string(scope).map(|s| s.to_rust_string_lossy(scope)),
                        val.to_string(scope).map(|s| s.to_rust_string_lossy(scope)),
                    ) {
                        pairs.push((k, v));
                    }
                }
            }
        }
        Some(pairs)
    });

    let result = oam_core::child::spawn_sync(
        &command,
        &child_args,
        cwd.as_deref(),
        env_pairs.as_deref(),
        input.as_deref(),
        shell,
        clear_env,
        timeout_ms,
        max_buffer,
    );

    let obj = v8::Object::new(scope);
    if let Some(stdout) = bytes_to_uint8array(scope, result.stdout) {
        let key = v8::String::new(scope, "stdout").unwrap();
        obj.set(scope, key.into(), stdout);
    }
    if let Some(stderr) = bytes_to_uint8array(scope, result.stderr) {
        let key = v8::String::new(scope, "stderr").unwrap();
        obj.set(scope, key.into(), stderr);
    }
    let key = v8::String::new(scope, "pid").unwrap();
    let val = v8::Number::new(scope, result.pid as f64);
    obj.set(scope, key.into(), val.into());

    let key = v8::String::new(scope, "status").unwrap();
    match result.status {
        Some(code) => {
            let val = v8::Number::new(scope, code as f64);
            obj.set(scope, key.into(), val.into());
        }
        None => {
            obj.set(scope, key.into(), v8::null(scope).into());
        }
    }

    let key = v8::String::new(scope, "signal").unwrap();
    match &result.signal {
        Some(sig) => {
            let val = v8::String::new(scope, sig).unwrap();
            obj.set(scope, key.into(), val.into());
        }
        None => {
            obj.set(scope, key.into(), v8::null(scope).into());
        }
    }

    if let Some(error) = &result.error {
        let err_obj = v8::Object::new(scope);
        let k = v8::String::new(scope, "code").unwrap();
        let v = v8::String::new(scope, &error.code).unwrap();
        err_obj.set(scope, k.into(), v.into());
        let k = v8::String::new(scope, "message").unwrap();
        let v = v8::String::new(scope, &error.message).unwrap();
        err_obj.set(scope, k.into(), v.into());
        let key = v8::String::new(scope, "error").unwrap();
        obj.set(scope, key.into(), err_obj.into());
    }

    rv.set(obj.into());
}

fn op_spawn_async(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(command) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "spawn: command required");
        return;
    };

    let args_val = args.get(1);
    let mut child_args: Vec<String> = Vec::new();
    if let Ok(arr) = v8::Local::<v8::Array>::try_from(args_val) {
        for i in 0..arr.length() {
            if let Some(v) = arr.get_index(scope, i) {
                if let Some(s) = v.to_string(scope) {
                    child_args.push(s.to_rust_string_lossy(scope));
                }
            }
        }
    }

    let opts_val = args.get(2);
    let opts = if opts_val.is_null_or_undefined() {
        None
    } else {
        v8::Local::<v8::Object>::try_from(opts_val).ok()
    };

    let cwd = opts.and_then(|o| {
        let key = v8::String::new(scope, "cwd")?;
        let val = o.get(scope, key.into())?;
        if val.is_null_or_undefined() { return None; }
        val.to_string(scope).map(|s| s.to_rust_string_lossy(scope))
    });
    let shell = opts
        .and_then(|o| {
            let key = v8::String::new(scope, "shell")?;
            o.get(scope, key.into())
        })
        .is_some_and(|v| v.is_true());
    let clear_env = opts
        .and_then(|o| {
            let key = v8::String::new(scope, "clearEnv")?;
            o.get(scope, key.into())
        })
        .is_some_and(|v| v.is_true());

    let env_pairs: Option<Vec<(String, String)>> = opts.and_then(|o| {
        let key = v8::String::new(scope, "env")?;
        let val = o.get(scope, key.into())?;
        if val.is_null_or_undefined() { return None; }
        let env_obj = v8::Local::<v8::Object>::try_from(val).ok()?;
        let names = env_obj.get_own_property_names(scope, Default::default())?;
        let mut pairs = Vec::new();
        for i in 0..names.length() {
            if let Some(name) = names.get_index(scope, i) {
                if let Some(val) = env_obj.get(scope, name) {
                    if let (Some(k), Some(v)) = (
                        name.to_string(scope).map(|s| s.to_rust_string_lossy(scope)),
                        val.to_string(scope).map(|s| s.to_rust_string_lossy(scope)),
                    ) {
                        pairs.push((k, v));
                    }
                }
            }
        }
        Some(pairs)
    });

    let children = scope
        .get_slot::<oam_core::CoreRuntime>()
        .expect("core runtime installed")
        .children();
    let ids = scope
        .get_slot::<oam_core::CoreRuntime>()
        .expect("core runtime installed")
        .body_ids();

    let children2 = children.clone();
    crate::ops::spawn_op(scope, &mut rv, async move {
        match oam_core::child::spawn_child(command, child_args, cwd, env_pairs, shell, clear_env).await {
            Ok((child, pid)) => {
                let handle = ids.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let mut guard = children2.lock().expect("child registry lock");
                guard.insert(handle, oam_core::child::ChildProcess { child, pid });
                let json = serde_json::json!({ "handle": handle, "pid": pid });
                oam_core::OpOutcome::Json(json.to_string())
            }
            Err(msg) => oam_core::OpOutcome::Failed(msg),
        }
    });
}

fn op_spawn_kill(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let children = scope
        .get_slot::<oam_core::CoreRuntime>()
        .expect("core runtime installed")
        .children();
    let mut guard = children.lock().expect("child registry lock");
    if let Some(cp) = guard.get_mut(&handle) {
        let _ = cp.child.start_kill();
    }
}

fn op_spawn_read_stdout(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let children = scope
        .get_slot::<oam_core::CoreRuntime>()
        .expect("core runtime installed")
        .children();
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::child::child_read_stdout(children, handle),
    );
}

fn op_spawn_read_stderr(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let children = scope
        .get_slot::<oam_core::CoreRuntime>()
        .expect("core runtime installed")
        .children();
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::child::child_read_stderr(children, handle),
    );
}

fn op_spawn_write(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let Some(data) = arg_bytes(scope, &args, 1) else {
        throw_type_error(scope, "spawnWrite: data required");
        return;
    };
    let children = scope
        .get_slot::<oam_core::CoreRuntime>()
        .expect("core runtime installed")
        .children();
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::child::child_write_stdin(children, handle, data),
    );
}

fn op_spawn_wait(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let children = scope
        .get_slot::<oam_core::CoreRuntime>()
        .expect("core runtime installed")
        .children();
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::child::child_wait(children, handle),
    );
}

// ================================================================ dns

fn op_dns_lookup(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let hostname: String = args.get(0).to_rust_string_lossy(scope);

    let family = if args.get(1).is_number() {
        args.get(1).number_value(scope).unwrap_or(0.0) as i32
    } else {
        0
    };
    let all = args.get(2).is_true();

    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::dns::dns_lookup(hostname, family, all),
    );
}

// ============================================================== stdin

fn op_stdin_read(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    crate::ops::spawn_op(scope, &mut rv, oam_core::stdin_read());
}

// ============================================================== os extended

fn op_os_release(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let release = os_release();
    let val = v8::String::new(scope, &release).unwrap();
    rv.set(val.into());
}

fn op_os_total_mem(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    rv.set(v8::Number::new(scope, total_mem() as f64).into());
}

fn op_os_free_mem(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    rv.set(v8::Number::new(scope, free_mem() as f64).into());
}

#[cfg(windows)]
fn os_release() -> String {
    #[repr(C)]
    #[allow(non_snake_case)]
    struct OSVERSIONINFOW {
        dwOSVersionInfoSize: u32,
        dwMajorVersion: u32,
        dwMinorVersion: u32,
        dwBuildNumber: u32,
        dwPlatformId: u32,
        szCSDVersion: [u16; 128],
    }

    unsafe extern "system" {
        fn RtlGetVersion(info: *mut OSVERSIONINFOW) -> i32;
    }

    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
        dwMajorVersion: 0,
        dwMinorVersion: 0,
        dwBuildNumber: 0,
        dwPlatformId: 0,
        szCSDVersion: [0; 128],
    };
    unsafe { RtlGetVersion(&mut info) };
    format!("{}.{}.{}", info.dwMajorVersion, info.dwMinorVersion, info.dwBuildNumber)
}

#[cfg(not(windows))]
fn os_release() -> String {
    use std::process::Command;
    Command::new("uname").arg("-r").output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

#[cfg(windows)]
#[repr(C)]
#[allow(non_snake_case)]
struct MEMORYSTATUSEX {
    dwLength: u32,
    dwMemoryLoad: u32,
    ullTotalPhys: u64,
    ullAvailPhys: u64,
    ullTotalPageFile: u64,
    ullAvailPageFile: u64,
    ullTotalVirtual: u64,
    ullAvailVirtual: u64,
    ullAvailExtendedVirtual: u64,
}

#[cfg(windows)]
unsafe extern "system" {
    fn GlobalMemoryStatusEx(lpBuffer: *mut MEMORYSTATUSEX) -> i32;
}

#[cfg(windows)]
fn mem_status() -> MEMORYSTATUSEX {
    let mut status = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        dwMemoryLoad: 0,
        ullTotalPhys: 0,
        ullAvailPhys: 0,
        ullTotalPageFile: 0,
        ullAvailPageFile: 0,
        ullTotalVirtual: 0,
        ullAvailVirtual: 0,
        ullAvailExtendedVirtual: 0,
    };
    unsafe { GlobalMemoryStatusEx(&mut status) };
    status
}

#[cfg(windows)]
fn total_mem() -> u64 {
    mem_status().ullTotalPhys
}

#[cfg(windows)]
fn free_mem() -> u64 {
    mem_status().ullAvailPhys
}

#[cfg(not(windows))]
fn total_mem() -> u64 {
    use std::fs;
    fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("MemTotal:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|n| n.parse::<u64>().ok())
                .map(|kb| kb * 1024)
        })
        .unwrap_or(0)
}

#[cfg(not(windows))]
fn free_mem() -> u64 {
    use std::fs;
    fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("MemAvailable:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|n| n.parse::<u64>().ok())
                .map(|kb| kb * 1024)
        })
        .unwrap_or(0)
}

// ======================================================== V8 heap statistics

fn op_heap_statistics(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let stats = scope.get_heap_statistics();
    let obj = v8::Object::new(scope);
    let pairs: &[(&str, usize)] = &[
        ("total_heap_size", stats.total_heap_size()),
        ("total_heap_size_executable", stats.total_heap_size_executable()),
        ("total_physical_size", stats.total_physical_size()),
        ("total_available_size", stats.total_available_size()),
        ("used_heap_size", stats.used_heap_size()),
        ("heap_size_limit", stats.heap_size_limit()),
        ("malloced_memory", stats.malloced_memory()),
        ("peak_malloced_memory", stats.peak_malloced_memory()),
        ("does_zap_garbage", if stats.does_zap_garbage() { 1 } else { 0 }),
        ("number_of_native_contexts", stats.number_of_native_contexts()),
        ("number_of_detached_contexts", stats.number_of_detached_contexts()),
        ("external_memory", stats.external_memory()),
    ];
    for (key, val) in pairs {
        let k = v8::String::new(scope, key).unwrap();
        let v = v8::Number::new(scope, *val as f64);
        obj.set(scope, k.into(), v.into());
    }
    rv.set(obj.into());
}

// ============================================================= process RSS

fn op_process_rss(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    rv.set(v8::Number::new(scope, process_rss() as f64).into());
}

#[cfg(windows)]
fn process_rss() -> usize {
    #[repr(C)]
    #[allow(non_snake_case)]
    struct PROCESS_MEMORY_COUNTERS {
        cb: u32,
        PageFaultCount: u32,
        PeakWorkingSetSize: usize,
        WorkingSetSize: usize,
        QuotaPeakPagedPoolUsage: usize,
        QuotaPagedPoolUsage: usize,
        QuotaPeakNonPagedPoolUsage: usize,
        QuotaNonPagedPoolUsage: usize,
        PagefileUsage: usize,
        PeakPagefileUsage: usize,
    }

    unsafe extern "system" {
        fn K32GetProcessMemoryInfo(
            hProcess: isize,
            ppsmemCounters: *mut PROCESS_MEMORY_COUNTERS,
            cb: u32,
        ) -> i32;
        fn GetCurrentProcess() -> isize;
    }

    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        PageFaultCount: 0,
        PeakWorkingSetSize: 0,
        WorkingSetSize: 0,
        QuotaPeakPagedPoolUsage: 0,
        QuotaPagedPoolUsage: 0,
        QuotaPeakNonPagedPoolUsage: 0,
        QuotaNonPagedPoolUsage: 0,
        PagefileUsage: 0,
        PeakPagefileUsage: 0,
    };
    unsafe {
        K32GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters,
            counters.cb,
        );
    }
    counters.WorkingSetSize
}

#[cfg(not(windows))]
fn process_rss() -> usize {
    use std::fs;
    fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1)?.parse::<usize>().ok())
        .map(|pages| pages * 4096)
        .unwrap_or(0)
}

// ============================================================= CPU info

fn op_cpu_model(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let model = cpu_model();
    let val = v8::String::new(scope, &model).unwrap();
    rv.set(val.into());
}

fn op_cpu_speed(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    rv.set(v8::Number::new(scope, cpu_speed_mhz() as f64).into());
}

#[cfg(windows)]
fn cpu_model() -> String {
    use std::ptr;

    const KEY_READ: u32 = 0x20019;
    const HKEY_LOCAL_MACHINE: isize = 0x80000002u32 as i32 as isize;

    unsafe extern "system" {
        fn RegOpenKeyExW(hKey: isize, lpSubKey: *const u16, ulOptions: u32, samDesired: u32, phkResult: *mut isize) -> i32;
        fn RegQueryValueExW(hKey: isize, lpValueName: *const u16, lpReserved: *mut u32, lpType: *mut u32, lpData: *mut u8, lpcbData: *mut u32) -> i32;
        fn RegCloseKey(hKey: isize) -> i32;
    }

    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    let sub_key = to_wide("HARDWARE\\DESCRIPTION\\System\\CentralProcessor\\0");
    let value_name = to_wide("ProcessorNameString");
    let mut hkey: isize = 0;

    if unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, sub_key.as_ptr(), 0, KEY_READ, &mut hkey) } != 0 {
        return std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_default();
    }

    let mut buf = vec![0u8; 256];
    let mut buf_len = buf.len() as u32;
    let mut reg_type: u32 = 0;

    let result = unsafe {
        RegQueryValueExW(
            hkey, value_name.as_ptr(), ptr::null_mut(), &mut reg_type,
            buf.as_mut_ptr(), &mut buf_len,
        )
    };
    unsafe { RegCloseKey(hkey) };

    if result != 0 || buf_len < 2 {
        return std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_default();
    }

    let wide: Vec<u16> = buf[..buf_len as usize]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&c| c != 0)
        .collect();
    String::from_utf16_lossy(&wide).trim().to_string()
}

#[cfg(windows)]
fn cpu_speed_mhz() -> u32 {
    use std::ptr;

    const KEY_READ: u32 = 0x20019;
    const HKEY_LOCAL_MACHINE: isize = 0x80000002u32 as i32 as isize;

    unsafe extern "system" {
        fn RegOpenKeyExW(hKey: isize, lpSubKey: *const u16, ulOptions: u32, samDesired: u32, phkResult: *mut isize) -> i32;
        fn RegQueryValueExW(hKey: isize, lpValueName: *const u16, lpReserved: *mut u32, lpType: *mut u32, lpData: *mut u8, lpcbData: *mut u32) -> i32;
        fn RegCloseKey(hKey: isize) -> i32;
    }

    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    let sub_key = to_wide("HARDWARE\\DESCRIPTION\\System\\CentralProcessor\\0");
    let value_name = to_wide("~MHz");
    let mut hkey: isize = 0;

    if unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, sub_key.as_ptr(), 0, KEY_READ, &mut hkey) } != 0 {
        return 0;
    }

    let mut val: u32 = 0;
    let mut val_len = 4u32;
    let mut reg_type: u32 = 0;

    let result = unsafe {
        RegQueryValueExW(
            hkey, value_name.as_ptr(), ptr::null_mut(), &mut reg_type,
            &mut val as *mut u32 as *mut u8, &mut val_len,
        )
    };
    unsafe { RegCloseKey(hkey) };

    if result != 0 { 0 } else { val }
}

#[cfg(not(windows))]
fn cpu_model() -> String {
    use std::fs;
    fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split(':').nth(1))
                .map(|v| v.trim().to_string())
        })
        .unwrap_or_default()
}

#[cfg(not(windows))]
fn cpu_speed_mhz() -> u32 {
    use std::fs;
    fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("cpu MHz"))
                .and_then(|l| l.split(':').nth(1))
                .and_then(|v| v.trim().parse::<f64>().ok())
                .map(|f| f as u32)
        })
        .unwrap_or(0)
}

// ====================================================== network interfaces

fn op_network_interfaces(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let json = serde_json::to_string(&network_interfaces()).unwrap_or_else(|_| "{}".into());
    let val = v8::String::new(scope, &json).unwrap();
    rv.set(val.into());
}

fn network_interfaces() -> serde_json::Value {
    use serde_json::{json, Map, Value};
    use std::net::UdpSocket;

    let mut result = Map::new();

    let lo_name = if cfg!(windows) { "Loopback Pseudo-Interface 1" } else { "lo" };
    result.insert(lo_name.to_string(), json!([
        {
            "address": "127.0.0.1",
            "netmask": "255.0.0.0",
            "family": "IPv4",
            "mac": "00:00:00:00:00:00",
            "internal": true,
            "cidr": "127.0.0.1/8"
        },
        {
            "address": "::1",
            "netmask": "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "family": "IPv6",
            "mac": "00:00:00:00:00:00",
            "internal": true,
            "cidr": "::1/128",
            "scopeid": 0
        }
    ]));

    if let Ok(socket) = UdpSocket::bind("0.0.0.0:0") {
        if socket.connect("8.8.8.8:80").is_ok() {
            if let Ok(addr) = socket.local_addr() {
                let ip = addr.ip().to_string();
                let iface_name = if cfg!(windows) { "Ethernet" } else { "eth0" };
                result.insert(iface_name.to_string(), json!([
                    {
                        "address": ip,
                        "netmask": "255.255.255.0",
                        "family": "IPv4",
                        "mac": "00:00:00:00:00:00",
                        "internal": false,
                        "cidr": format!("{}/24", ip)
                    }
                ]));
            }
        }
    }

    Value::Object(result)
}

// ============================================================= parent PID

#[cfg(windows)]
fn parent_pid() -> u32 {
    #[repr(C)]
    #[allow(non_snake_case)]
    struct PROCESS_BASIC_INFORMATION {
        ExitStatus: isize,
        PebBaseAddress: *mut u8,
        AffinityMask: usize,
        BasePriority: i32,
        UniqueProcessId: usize,
        InheritedFromUniqueProcessId: usize,
    }

    unsafe extern "system" {
        fn NtQueryInformationProcess(
            ProcessHandle: isize,
            ProcessInformationClass: u32,
            ProcessInformation: *mut u8,
            ProcessInformationLength: u32,
            ReturnLength: *mut u32,
        ) -> i32;
        fn GetCurrentProcess() -> isize;
    }

    let mut pbi = PROCESS_BASIC_INFORMATION {
        ExitStatus: 0,
        PebBaseAddress: std::ptr::null_mut(),
        AffinityMask: 0,
        BasePriority: 0,
        UniqueProcessId: 0,
        InheritedFromUniqueProcessId: 0,
    };
    let mut ret_len: u32 = 0;
    unsafe {
        NtQueryInformationProcess(
            GetCurrentProcess(),
            0,
            &mut pbi as *mut _ as *mut u8,
            std::mem::size_of::<PROCESS_BASIC_INFORMATION>() as u32,
            &mut ret_len,
        );
    }
    pbi.InheritedFromUniqueProcessId as u32
}

#[cfg(not(windows))]
fn parent_pid() -> u32 {
    use std::fs;
    fs::read_to_string("/proc/self/stat")
        .ok()
        .and_then(|s| {
            let after_comm = s.rfind(')')?;
            s[after_comm + 2..].split_whitespace().nth(1)?.parse().ok()
        })
        .unwrap_or(0)
}

// ========================================================== process.cpuUsage

fn op_process_cpu_usage(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let (user, system) = cpu_usage_us();
    let json = format!("{{\"user\":{user},\"system\":{system}}}");
    return_json(scope, &mut rv, &json);
}

#[cfg(windows)]
fn cpu_usage_us() -> (u64, u64) {
    #[repr(C)]
    struct FILETIME {
        lo: u32,
        hi: u32,
    }
    impl FILETIME {
        fn to_us(&self) -> u64 {
            let ticks = (self.hi as u64) << 32 | self.lo as u64;
            ticks / 10
        }
    }

    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
        fn GetProcessTimes(
            h: isize,
            creation: *mut FILETIME,
            exit: *mut FILETIME,
            kernel: *mut FILETIME,
            user: *mut FILETIME,
        ) -> i32;
    }

    let mut creation = FILETIME { lo: 0, hi: 0 };
    let mut exit = FILETIME { lo: 0, hi: 0 };
    let mut kernel = FILETIME { lo: 0, hi: 0 };
    let mut user = FILETIME { lo: 0, hi: 0 };
    unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        );
    }
    (user.to_us(), kernel.to_us())
}

#[cfg(not(windows))]
fn cpu_usage_us() -> (u64, u64) {
    #[repr(C)]
    struct timeval {
        tv_sec: i64,
        tv_usec: i64,
    }
    #[repr(C)]
    struct rusage {
        ru_utime: timeval,
        ru_stime: timeval,
        _pad: [u8; 112],
    }
    unsafe extern "C" {
        fn getrusage(who: i32, usage: *mut rusage) -> i32;
    }
    let mut usage = rusage {
        ru_utime: timeval { tv_sec: 0, tv_usec: 0 },
        ru_stime: timeval { tv_sec: 0, tv_usec: 0 },
        _pad: [0; 112],
    };
    unsafe { getrusage(0, &mut usage) };
    let user = usage.ru_utime.tv_sec as u64 * 1_000_000 + usage.ru_utime.tv_usec as u64;
    let system = usage.ru_stime.tv_sec as u64 * 1_000_000 + usage.ru_stime.tv_usec as u64;
    (user, system)
}

// ========================================================== process.kill

fn op_process_kill(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let pid = args.get(0).number_value(scope).unwrap_or(0.0) as u32;
    let signal = args.get(1).number_value(scope).unwrap_or(15.0) as i32;
    if let Err(msg) = kill_process(pid, signal) {
        throw_type_error(scope, &msg);
    }
}

#[cfg(windows)]
fn kill_process(pid: u32, signal: i32) -> Result<(), String> {
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
        fn TerminateProcess(handle: isize, exit_code: u32) -> i32;
        fn CloseHandle(handle: isize) -> i32;
    }
    if signal == 0 {
        let handle = unsafe { OpenProcess(0x1000, 0, pid) };
        if handle == 0 {
            return Err(format!("kill: no such process {pid}"));
        }
        unsafe { CloseHandle(handle) };
        return Ok(());
    }
    let handle = unsafe { OpenProcess(0x0001, 0, pid) };
    if handle == 0 {
        return Err(format!("kill: no such process {pid}"));
    }
    let ok = unsafe { TerminateProcess(handle, 1) };
    unsafe { CloseHandle(handle) };
    if ok == 0 {
        return Err(format!("kill: could not terminate process {pid}"));
    }
    Ok(())
}

#[cfg(not(windows))]
fn kill_process(pid: u32, signal: i32) -> Result<(), String> {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    let ret = unsafe { kill(pid as i32, signal) };
    if ret != 0 {
        let errno = std::io::Error::last_os_error();
        return Err(format!("kill({pid}, {signal}): {errno}"));
    }
    Ok(())
}
