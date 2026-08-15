//! Native bindings for the node: compat layer: `__oam.node`.
//!
//! The JS half (js/node_compat.js factories) is pure and lives in the
//! snapshot; everything here is installed after restore. Sync fs natives
//! call std::fs directly on the isolate thread (that is what Sync means);
//! async fs natives ride the oam_core op channel like fetch does. All fs
//! failures throw/reject Errors carrying Node's `.code` (ENOENT, ...) —
//! the ecosystem branches on codes, not messages.

use crate::crypto_ops::{
    op_crypto_check_prime, op_crypto_cipher_create, op_crypto_cipher_final,
    op_crypto_cipher_final_gcm, op_crypto_cipher_get_auth_tag, op_crypto_cipher_set_aad,
    op_crypto_cipher_set_auth_tag, op_crypto_cipher_set_auto_padding, op_crypto_cipher_update,
    op_crypto_dh_compute_secret, op_crypto_dh_generate_keys, op_crypto_ec_jwk_export,
    op_crypto_ec_jwk_import, op_crypto_ecdh_compute_secret, op_crypto_ecdh_generate_keys,
    op_crypto_ecdh_get_public_key, op_crypto_extract_public_pem, op_crypto_generate_keypair,
    op_crypto_generate_prime, op_crypto_hash_copy, op_crypto_hash_create, op_crypto_hash_digest,
    op_crypto_hash_update, op_crypto_hkdf_sync, op_crypto_hmac_create, op_crypto_pbkdf2_sync,
    op_crypto_private_decrypt, op_crypto_private_encrypt, op_crypto_public_decrypt,
    op_crypto_public_encrypt, op_crypto_random_fill, op_crypto_rsa_jwk_components,
    op_crypto_scrypt_sync, op_crypto_sign, op_crypto_sign_pss, op_crypto_timing_safe_equal,
    op_crypto_verify, op_crypto_verify_pss, op_crypto_x509_parse,
};
use crate::timers::{timer_ref, timer_unref};
use crate::vm_context::{
    op_vm_compile, op_vm_create_context, op_vm_is_context, op_vm_run_in_context,
    op_vm_run_in_this_context,
};
use crate::vm_module::{
    op_vm_module_compile, op_vm_module_error, op_vm_module_evaluate, op_vm_module_instantiate,
    op_vm_module_link, op_vm_module_namespace, op_vm_module_release, op_vm_module_requests,
    op_vm_module_status,
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
    let data: [(&str, v8::Local<v8::Value>); 8] = [
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
            // process.execPath is the REAL path of the running binary, not
            // argv[0]: launched through a symlink, Node still reports the
            // resolved target (argv[0] keeps the invoked name, and that is
            // what process.argv0 is for). Canonicalized on unix only --
            // on Windows it would add a \\?\ verbatim prefix that nothing
            // else in the ecosystem expects.
            "execPath",
            {
                let exe = std::env::current_exe().unwrap_or_default();
                #[cfg(unix)]
                let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
                v8::String::new(scope, &exe.to_string_lossy())
                    .unwrap_or_else(|| v8::String::empty(scope))
                    .into()
            },
        ),
        (
            // Real shipped dependency versions (JSON), read from Cargo.lock
            // by build.rs -- the honest tail of process.versions.
            "depVersions",
            v8::String::new(scope, env!("OAM_DEP_VERSIONS"))
                .unwrap()
                .into(),
        ),
        (
            "pid",
            v8::Number::new(scope, std::process::id() as f64).into(),
        ),
        ("ppid", v8::Number::new(scope, parent_pid() as f64).into()),
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
        // TTY raw-mode + window-size for interactive TUIs (process.stdin/stdout).
        ("ttySetRawMode", op_tty_set_raw_mode),
        ("ttyGetWinSize", op_tty_get_win_size),
        // POSIX identity. Real syscalls -- a hardcoded uid of 0 would claim
        // the process is root, which is both false and load-bearing (code
        // branches on `getuid() === 0`). Absent on Windows, where Node does
        // not define these at all.
        ("vmCompile", op_vm_compile),
        ("vmModuleCompile", op_vm_module_compile),
        ("vmModuleRequests", op_vm_module_requests),
        ("vmModuleLink", op_vm_module_link),
        ("vmModuleInstantiate", op_vm_module_instantiate),
        ("vmModuleEvaluate", op_vm_module_evaluate),
        ("vmModuleNamespace", op_vm_module_namespace),
        ("vmModuleStatus", op_vm_module_status),
        ("vmModuleError", op_vm_module_error),
        ("vmModuleRelease", op_vm_module_release),
        ("vmCreateContext", op_vm_create_context),
        ("vmIsContext", op_vm_is_context),
        ("vmRunInContext", op_vm_run_in_context),
        ("vmRunInThisContext", op_vm_run_in_this_context),
        ("v8Is", op_v8_is),
        ("posixGetId", op_posix_get_id),
        ("posixSetId", op_posix_set_id),
        ("posixGetGroups", op_posix_get_groups),
        ("posixSetGroups", op_posix_set_groups),
        ("posixInitGroups", op_posix_init_groups),
        ("posixLookupId", op_posix_lookup_id),
        ("setTimeZone", op_set_timezone),
        ("processExecve", op_process_execve),
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
        // Timer ref/unref: flip the native TimerQueue ref flag so an unref'd
        // timer no longer keeps the event loop alive (Node Timeout#ref/#unref).
        ("timerRef", timer_ref),
        ("timerUnref", timer_unref),
        // Inbound OS signals: install/remove native delivery of a Node signal
        // name (SIGTERM/SIGINT/SIGHUP/...). Gated JS-side on process
        // listenerCount so start fires on the FIRST listener and stop on the
        // LAST (js/node_compat.js newListener/removeListener wiring).
        ("startSignal", op_start_signal),
        ("stopSignal", op_stop_signal),
        // V8 Object::GetConstructorName -- used by util.inspect for null-proto labels.
        ("getConstructorName", op_get_constructor_name),
        // fs sync
        ("fsReadFileSync", op_fs_read_file_sync),
        ("fsReadFileUtf8Sync", op_fs_read_file_utf8_sync),
        ("fsWriteFileSync", op_fs_write_file_sync),
        ("fsExistsSync", op_fs_exists_sync),
        ("fsStatSync", op_fs_stat_sync),
        ("fsStatfsSync", op_fs_statfs_sync),
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
        ("fsFstat", op_fs_fstat),
        ("fsStatfs", op_fs_statfs),
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
        // fs classic synchronous fd ops (openSync/readSync/writeSync/...)
        ("fsOpenSync", op_fs_open_sync),
        ("fsReadSync", op_fs_read_sync),
        ("fsWriteSync", op_fs_write_sync),
        ("fsCloseSync", op_fs_close_sync),
        ("fsFstatSync", op_fs_fstat_sync),
        // fs fd-based ops (fsync/ftruncate/fchmod/fchown/futimes families)
        ("fsFsync", op_fs_fsync),
        ("fsFdatasync", op_fs_fdatasync),
        ("fsFtruncate", op_fs_ftruncate),
        ("fsFchmod", op_fs_fchmod),
        ("fsFchown", op_fs_fchown),
        ("fsFutimes", op_fs_futimes),
        ("fsFsyncSync", op_fs_fsync_sync),
        ("fsFdatasyncSync", op_fs_fdatasync_sync),
        ("fsFtruncateSync", op_fs_ftruncate_sync),
        ("fsFchmodSync", op_fs_fchmod_sync),
        ("fsFchownSync", op_fs_fchown_sync),
        ("fsFutimesSync", op_fs_futimes_sync),
        // fs path-based ownership/time (chown/lchown/utimes/lutimes/lchmod)
        ("fsChown", op_fs_chown),
        ("fsLchown", op_fs_lchown),
        ("fsUtimes", op_fs_utimes),
        ("fsLutimes", op_fs_lutimes),
        ("fsLchmod", op_fs_lchmod),
        ("fsChownSync", op_fs_chown_sync),
        ("fsLchownSync", op_fs_lchown_sync),
        ("fsUtimesSync", op_fs_utimes_sync),
        ("fsLutimesSync", op_fs_lutimes_sync),
        ("fsLchmodSync", op_fs_lchmod_sync),
        // node:zlib (one-shot)
        ("zlibSync", op_zlib_sync),
        ("zlibAsync", op_zlib_async),
        // node:zlib streaming (incremental Transform backing)
        ("zlibStreamCreate", op_zlib_stream_create),
        ("zlibStreamWrite", op_zlib_stream_write),
        ("zlibStreamFlush", op_zlib_stream_flush),
        ("zlibStreamClose", op_zlib_stream_close),
        // node:zlib handle (sync incremental, for ssh2/native binding compat)
        ("zlibHandleCreate", op_zlib_handle_create),
        ("zlibHandleWriteSync", op_zlib_handle_write_sync),
        // HTTP server
        ("httpServe", op_http_serve),
        ("httpAccept", op_http_accept),
        ("httpRequestBody", op_http_request_body),
        ("httpRespond", op_http_respond),
        ("httpRespondStream", op_http_respond_stream),
        ("httpBodyPush", op_http_body_push),
        ("httpBodyEnd", op_http_body_end),
        ("httpStreamClosed", op_http_stream_closed),
        ("httpRequestBodyRead", op_http_request_body_read),
        ("fetchBodyChannelNew", op_fetch_body_channel_new),
        ("fetchBodyChannelWrite", op_fetch_body_channel_write),
        ("fetchBodyChannelEnd", op_fetch_body_channel_end),
        ("fetchBodyChannelCancel", op_fetch_body_channel_cancel),
        ("httpRequestBodyCancel", op_http_request_body_cancel),
        ("httpAbort", op_http_abort),
        ("httpClose", op_http_close),
        // HTTP/2 cleartext server (h2c — same accept/respond ops)
        ("http2Serve", op_http2_serve),
        // HTTPS server (TLS-wrapped HTTP, same accept/respond ops)
        ("httpsServe", op_https_serve),
        // TCP sockets (node:net)
        ("tcpConnect", op_tcp_connect),
        ("tcpRead", op_tcp_read),
        ("tcpWrite", op_tcp_write),
        ("tcpClose", op_tcp_close),
        ("tcpShutdown", op_tcp_shutdown),
        ("tcpListen", op_tcp_listen),
        ("tcpAccept", op_tcp_accept),
        ("tcpServerClose", op_tcp_server_close),
        // UDP sockets (node:dgram)
        ("udpBind", op_udp_bind),
        ("udpSend", op_udp_send),
        ("udpRecv", op_udp_recv),
        ("udpClose", op_udp_close),
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
        ("cryptoPrivateEncrypt", op_crypto_private_encrypt),
        ("cryptoPublicDecrypt", op_crypto_public_decrypt),
        ("cryptoExtractPublicPem", op_crypto_extract_public_pem),
        ("cryptoRsaJwkComponents", op_crypto_rsa_jwk_components),
        // node:crypto wave 9: EC JWK import/export + RSA-PSS
        ("cryptoEcJwkImport", op_crypto_ec_jwk_import),
        ("cryptoEcJwkExport", op_crypto_ec_jwk_export),
        ("cryptoSignPss", op_crypto_sign_pss),
        ("cryptoVerifyPss", op_crypto_verify_pss),
        // node:crypto wave 5: ECDH key agreement
        ("cryptoEcdhGenerateKeys", op_crypto_ecdh_generate_keys),
        ("cryptoEcdhComputeSecret", op_crypto_ecdh_compute_secret),
        ("cryptoEcdhGetPublicKey", op_crypto_ecdh_get_public_key),
        ("cryptoDhGenerateKeys", op_crypto_dh_generate_keys),
        ("cryptoDhComputeSecret", op_crypto_dh_compute_secret),
        ("cryptoX509Parse", op_crypto_x509_parse),
        ("cryptoGeneratePrime", op_crypto_generate_prime),
        ("cryptoCheckPrime", op_crypto_check_prime),
        // TLS sockets (node:tls)
        ("tlsConnect", op_tls_connect),
        ("tlsRead", op_tls_read),
        ("tlsWrite", op_tls_write),
        ("tlsClose", op_tls_close),
        ("tlsShutdown", op_tls_shutdown),
        ("tlsAcceptWrap", op_tls_accept_wrap),
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
        ("spawnCloseStdin", op_spawn_close_stdin),
        ("spawnWait", op_spawn_wait),
        // Windows extra-fd stdio (CDP-over-pipe for browsers). Real on Windows,
        // platform-error stubs elsewhere.
        ("spawnExtra", op_spawn_extra),
        ("spawnExtraRead", op_spawn_extra_read),
        ("spawnExtraWrite", op_spawn_extra_write),
        ("spawnExtraCloseFd", op_spawn_extra_close_fd),
        ("spawnExtraWait", op_spawn_extra_wait),
        ("spawnExtraKill", op_spawn_extra_kill),
        // cluster
        ("clusterFork", op_cluster_fork),
        ("clusterWorkerWait", op_cluster_worker_wait),
        ("clusterWorkerKill", op_cluster_worker_kill),
        ("clusterIsWorker", op_cluster_is_worker),
        // dns
        ("dnsLookup", op_dns_lookup),
        ("dnsResolve", op_dns_resolve),
        ("dnsReverse", op_dns_reverse),
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

/// Safely create a V8 string from a dynamic (possibly very large) Rust
/// string. V8 rejects strings longer than ~1 GB; the fallback avoids a
/// panic on the FFI boundary.
#[allow(dead_code)]
fn v8_string_or_fallback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    s: &str,
) -> v8::Local<'s, v8::String> {
    v8::String::new(scope, s).unwrap_or_else(|| v8::String::new(scope, "internal error").unwrap())
}

macro_rules! core_runtime {
    ($scope:expr) => {
        match $scope.get_slot::<oam_core::CoreRuntime>() {
            Some(rt) => rt,
            None => {
                throw_type_error($scope, "internal: runtime not initialized");
                return;
            }
        }
    };
}

#[allow(unused_macros)]
macro_rules! core_runtime_mut {
    ($scope:expr) => {
        match $scope.get_slot_mut::<oam_core::CoreRuntime>() {
            Some(rt) => rt,
            None => {
                throw_type_error($scope, "internal: runtime not initialized");
                return;
            }
        }
    };
}

/// Throw an Error with Node's `.code` / `.syscall` / `.path` properties.
/// Node's `err.errno`. Single source of truth lives in oam_core so the
/// child-spawn failure body can use the same table; see its doc comment.
fn node_errno(code: &str, error: &std::io::Error) -> Option<i32> {
    oam_core::node_errno(code, error)
}

/// Throw node's system-error shape for a PATH operation.
///
/// `path` is reported verbatim, including when it is genuinely empty:
/// `fs.openSync("")` gives node `ENOENT: no such file or directory, open ''`
/// with `path: ''` present. For an operation on a descriptor, which has no
/// path at all, use `throw_fd_error` -- inferring "no path" from `path == ""`
/// conflated the two and stripped the quotes off every empty-path error.
fn throw_node_error(
    scope: &mut v8::PinScope<'_, '_>,
    syscall: &str,
    path: &str,
    error: &std::io::Error,
) {
    let code = node_error_code(error);
    let message = node_error_message(code, syscall, path, error);
    throw_system_error(scope, code, &message, syscall, Some(path), error);
}

/// Throw node's system-error shape for an operation on an ALREADY-OPEN
/// descriptor: no `path` property, no path segment in the message, and a
/// wrong-mode denial reported as EBADF rather than EACCES (see
/// `oam_core::fd_error_code`).
fn throw_fd_error(scope: &mut v8::PinScope<'_, '_>, syscall: &str, error: &std::io::Error) {
    let code = oam_core::fd_error_code(error);
    let message = oam_core::node_error_message_fd(code, syscall, error);
    throw_system_error(scope, code, &message, syscall, None, error);
}

fn throw_system_error(
    scope: &mut v8::PinScope<'_, '_>,
    code: &str,
    message: &str,
    syscall: &str,
    path: Option<&str>,
    error: &std::io::Error,
) {
    let message_v8 =
        v8::String::new(scope, message).unwrap_or_else(|| v8::String::new(scope, code).unwrap());
    let exception = v8::Exception::error(scope, message_v8);
    if let Ok(obj) = v8::Local::<v8::Object>::try_from(exception) {
        // ORDER IS PART OF THE SHAPE. `Object.keys(err)` is observable, and
        // node's fs errors enumerate errno, code, syscall, path -- measured on
        // a closed-fd readSync: node gives ["errno","code","syscall"] where oam
        // gave ["code","syscall","errno"]. Anything that snapshots or diffs an
        // error object (a test, a serialiser, a reporter) sees the difference,
        // so errno is set FIRST rather than appended after the string props.
        if let Some(errno) = node_errno(code, error)
            && let Some(key) = v8::String::new(scope, "errno")
        {
            // A Number, not a string: node's negative libuv error number.
            let val = v8::Integer::new(scope, errno);
            obj.set(scope, key.into(), val.into());
        }
        // `path` is present only for a path operation. An fd error has no path
        // in node at all -- it is not an empty one -- and emitting `path: ''`
        // put a fourth key on `Object.keys(err)` that node never has.
        let props = [
            Some(("code", code)),
            Some(("syscall", syscall)),
            path.map(|p| ("path", p)),
        ];
        for (name, value) in props.into_iter().flatten() {
            let key = v8::String::new(scope, name).unwrap();
            if let Some(value) = v8::String::new(scope, value) {
                obj.set(scope, key.into(), value.into());
            }
        }
    }
    scope.throw_exception(exception);
}

/// An optional non-negative POSITION argument: a number seeks (pread/pwrite),
/// absent / null / negative means "from the current cursor". Shared by the four
/// read/write natives so the coercion cannot drift between them.
fn optional_position(
    scope: &mut v8::PinScope<'_, '_>,
    args: &v8::FunctionCallbackArguments<'_>,
    index: i32,
) -> Option<u64> {
    let value = args.get(index);
    if !value.is_number() {
        return None;
    }
    let p = value.number_value(scope).unwrap_or(-1.0);
    if p >= 0.0 { Some(p as u64) } else { None }
}

pub(crate) fn arg_string(
    scope: &mut v8::PinScope<'_, '_>,
    args: &v8::FunctionCallbackArguments<'_>,
    index: i32,
) -> Option<String> {
    let value = args.get(index);
    // A missing arg is `undefined`; v8's ToString would coerce that (and
    // `null`) to the literal "undefined"/"null", silently turning an absent
    // optional string into a present-but-bogus value. Treat null/undefined as
    // absent so optional args are truly `None`. (This coercion is what tripped
    // tls.connect's client-auth path -- a no-cert connection arrived as
    // Some("undefined") and failed `no private key found in client key PEM`.)
    if value.is_null_or_undefined() {
        return None;
    }
    value
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
    if value.is_undefined() || value.is_null() {
        return None;
    }
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
    // Filter by the `env` permission rather than throwing on a denied read.
    // process.env is handed to JS as a snapshot object, so there is no
    // per-property hook to throw from; a denied variable is simply absent and
    // reads as undefined. That also avoids leaking the names of variables the
    // script may not see, which an exception message would do.
    //
    // Collected before the object is built because get_permissions borrows the
    // scope immutably while v8::String::new needs it mutably.
    let allowed: Vec<(String, String)> = {
        let perms = get_permissions(scope);
        std::env::vars()
            .filter(|(name, _)| perms.check_env(name).is_ok())
            .collect()
    };
    let env = v8::Object::new(scope);
    for (name, value) in allowed {
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
    oam_core::exit_process(code);
}

fn op_stdout_write(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    // arg_bytes: views pass through VERBATIM (binary pipes stay binary),
    // strings encode as UTF-8.
    if let Some(bytes) = arg_bytes(scope, &args, 0) {
        // In a worker started with { stdout: true }, output belongs to the
        // parent's worker.stdout stream, not the shared process fd.
        if let Some(ctx) = scope.get_slot::<oam_core::worker::WorkerContext>()
            && ctx.pipe_stdout
        {
            let _ = ctx
                .outbox
                .send(oam_core::worker::WorkerEvent::Stdout(bytes));
            return;
        }
        use std::io::Write;
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        if let Err(e) = lock.write_all(&bytes).and_then(|_| lock.flush())
            && e.kind() == std::io::ErrorKind::BrokenPipe
        {
            oam_core::exit_process(0);
        }
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
        if let Err(e) = lock.write_all(&bytes).and_then(|_| lock.flush())
            && e.kind() == std::io::ErrorKind::BrokenPipe
        {
            oam_core::exit_process(0);
        }
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

// ============================================================ tty raw-mode
// process.stdin.setRawMode + stdout/stderr columns/rows. Windows uses inline
// console FFI (matches os_release/mem_status style); Unix uses libc termios +
// TIOCGWINSZ (cfg(unix) dep). Exactly one platform impl compiles per target;
// both are exercised by the 4-platform CI (only Windows builds locally).

#[cfg(windows)]
static WIN_STDIN_ORIG_MODE: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(u32::MAX);

#[cfg(unix)]
static UNIX_STDIN_ORIG_TERMIOS: std::sync::Mutex<Option<libc::termios>> =
    std::sync::Mutex::new(None);

#[cfg(windows)]
fn win_std_handle(fd: i32) -> isize {
    unsafe extern "system" {
        fn GetStdHandle(n_std_handle: u32) -> isize;
    }
    // (DWORD)-10 / -11 / -12 = STD_INPUT / STD_OUTPUT / STD_ERROR handle.
    let id: u32 = match fd {
        0 => 0xFFFF_FFF6,
        1 => 0xFFFF_FFF5,
        _ => 0xFFFF_FFF4,
    };
    unsafe { GetStdHandle(id) }
}

#[cfg(windows)]
fn tty_set_raw_mode(fd: i32, enable: bool) -> bool {
    use std::sync::atomic::Ordering;
    const ENABLE_PROCESSED_INPUT: u32 = 0x0001;
    const ENABLE_LINE_INPUT: u32 = 0x0002;
    const ENABLE_ECHO_INPUT: u32 = 0x0004;
    const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;
    unsafe extern "system" {
        fn GetConsoleMode(h: isize, mode: *mut u32) -> i32;
        fn SetConsoleMode(h: isize, mode: u32) -> i32;
    }
    let h = win_std_handle(fd);
    if h == 0 || h == -1 {
        return false;
    }
    unsafe {
        let mut mode: u32 = 0;
        if GetConsoleMode(h, &mut mode) == 0 {
            return false;
        }
        if enable {
            // Save the pre-raw mode once so setRawMode(false)/exit restores it.
            let _ = WIN_STDIN_ORIG_MODE.compare_exchange(
                u32::MAX,
                mode,
                Ordering::SeqCst,
                Ordering::SeqCst,
            );
            // Clearing PROCESSED_INPUT means Ctrl-C is NOT turned into a
            // CTRL_C_EVENT: it arrives as a 0x03 data byte and the console
            // ctrl handler (owned by the signals-in item) never fires -- this
            // is Node's documented raw-mode behavior and the coordination point.
            let raw = (mode & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT))
                | ENABLE_VIRTUAL_TERMINAL_INPUT;
            SetConsoleMode(h, raw) != 0
        } else {
            let orig = WIN_STDIN_ORIG_MODE.swap(u32::MAX, Ordering::SeqCst);
            let restore = if orig == u32::MAX {
                (mode | ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT)
                    & !ENABLE_VIRTUAL_TERMINAL_INPUT
            } else {
                orig
            };
            SetConsoleMode(h, restore) != 0
        }
    }
}

#[cfg(windows)]
fn tty_win_size(fd: i32) -> Option<(i32, i32)> {
    #[allow(dead_code)]
    #[repr(C)]
    struct Coord {
        x: i16,
        y: i16,
    }
    #[repr(C)]
    struct SmallRect {
        left: i16,
        top: i16,
        right: i16,
        bottom: i16,
    }
    #[allow(dead_code)]
    #[repr(C)]
    struct Csbi {
        size: Coord,
        cursor: Coord,
        attributes: u16,
        window: SmallRect,
        max_window: Coord,
    }
    unsafe extern "system" {
        fn GetConsoleScreenBufferInfo(h: isize, info: *mut Csbi) -> i32;
    }
    let h = win_std_handle(fd);
    if h == 0 || h == -1 {
        return None;
    }
    unsafe {
        let mut info: Csbi = std::mem::zeroed();
        if GetConsoleScreenBufferInfo(h, &mut info) == 0 {
            return None;
        }
        let cols = info.window.right as i32 - info.window.left as i32 + 1;
        let rows = info.window.bottom as i32 - info.window.top as i32 + 1;
        if cols <= 0 || rows <= 0 {
            return None;
        }
        Some((cols, rows))
    }
}

#[cfg(unix)]
fn tty_set_raw_mode(fd: i32, enable: bool) -> bool {
    unsafe {
        if enable {
            let mut term: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut term) != 0 {
                return false;
            }
            // Save the pre-raw termios once so setRawMode(false)/exit restores it.
            if let Ok(mut g) = UNIX_STDIN_ORIG_TERMIOS.lock()
                && g.is_none()
            {
                *g = Some(term);
            }
            let mut raw_term = term;
            // cfmakeraw == Node's raw mode (libuv uses it): clears
            // ICANON|ECHO|ISIG|IEXTEN, ICRNL|IXON etc, sets CS8, VMIN=1/VTIME=0.
            // With ISIG off, Ctrl-C arrives as a 0x03 data byte (no SIGINT) --
            // the coordination point with the signals-in item.
            libc::cfmakeraw(&mut raw_term);
            libc::tcsetattr(fd, libc::TCSANOW, &raw_term) == 0
        } else {
            let orig = UNIX_STDIN_ORIG_TERMIOS.lock().ok().and_then(|g| *g);
            match orig {
                Some(o) => libc::tcsetattr(fd, libc::TCSANOW, &o) == 0,
                None => true,
            }
        }
    }
}

#[cfg(unix)]
fn tty_win_size(fd: i32) -> Option<(i32, i32)> {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) != 0 {
            return None;
        }
        if ws.ws_col == 0 {
            return None;
        }
        Some((ws.ws_col as i32, ws.ws_row as i32))
    }
}

fn op_tty_set_raw_mode(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let fd = args.get(0).int32_value(scope).unwrap_or(0);
    let enable = args.get(1).boolean_value(scope);
    rv.set_bool(tty_set_raw_mode(fd, enable));
}

fn op_tty_get_win_size(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let fd = args.get(0).int32_value(scope).unwrap_or(1);
    match tty_win_size(fd) {
        Some((cols, rows)) => {
            let c = v8::Integer::new(scope, cols);
            let r = v8::Integer::new(scope, rows);
            let arr = v8::Array::new(scope, 2);
            arr.set_index(scope, 0, c.into());
            arr.set_index(scope, 1, r.into());
            rv.set(arr.into());
        }
        None => {
            rv.set(v8::null(scope).into());
        }
    }
}

/// POSIX identity ops. Every one is a real syscall on unix and a no-op that
/// reports "unsupported" on Windows, where the JS layer never installs the
/// corresponding `process.*` function in the first place (Node does not
/// define them there either).
///
/// `which`: 0=uid 1=gid 2=euid 3=egid.
/// `op_v8_is(value, kind) -> boolean`
///
/// V8's own answer to "what kind of value is this", for the `util.types`
/// predicates JS cannot decide honestly. `.constructor` and
/// `Symbol.toStringTag` are both forgeable -- node's util.types are V8 checks
/// precisely so a plain function wearing `AsyncFunction` as its constructor
/// does not pass as an async one -- and a prototype comparison would be
/// realm-bound. Unknown kinds answer false rather than throwing, so adding a
/// predicate JS-side cannot break an older binary.
fn op_v8_is(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let value = args.get(0);
    let Some(kind) = arg_string(scope, &args, 1) else {
        rv.set_bool(false);
        return;
    };
    let answer = match kind.as_str() {
        "asyncFunction" => value.is_async_function(),
        "generatorFunction" => value.is_generator_function(),
        "generatorObject" => value.is_generator_object(),
        "promise" => value.is_promise(),
        "proxy" => value.is_proxy(),
        "mapIterator" => value.is_map_iterator(),
        "setIterator" => value.is_set_iterator(),
        "moduleNamespaceObject" => value.is_module_namespace_object(),
        "external" => value.is_external(),
        "argumentsObject" => value.is_arguments_object(),
        "weakMap" => value.is_weak_map(),
        "weakSet" => value.is_weak_set(),
        _ => false,
    };
    rv.set_bool(answer);
}

fn op_posix_get_id(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let which = args.get(0).int32_value(scope).unwrap_or(0);
    #[cfg(unix)]
    {
        let id = unsafe {
            match which {
                0 => libc::getuid(),
                1 => libc::getgid(),
                2 => libc::geteuid(),
                _ => libc::getegid(),
            }
        };
        rv.set_double(f64::from(id));
    }
    #[cfg(not(unix))]
    {
        let _ = which;
        rv.set(v8::null(scope).into());
    }
}

/// Set a POSIX id. Returns null on success or the errno NAME on failure, so
/// JS can raise Node's `EPERM`-shaped error without a second op.
/// `which`: 0=uid 1=gid 2=euid 3=egid.
fn op_posix_set_id(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let which = args.get(0).int32_value(scope).unwrap_or(0);
    // uid_t is unsigned; a JS number arrives as f64 and must round-trip
    // without wrapping (-0 and values past 2^31-1 are explicitly exercised
    // by test-process-uid-gid).
    let raw = args.get(1).number_value(scope).unwrap_or(-1.0);
    #[cfg(unix)]
    {
        if !(0.0..=f64::from(u32::MAX)).contains(&raw) || raw.fract() != 0.0 {
            let msg = v8::String::new(scope, "EINVAL").expect("static errno string");
            rv.set(msg.into());
            return;
        }
        let id = raw as u32;
        let ok = unsafe {
            match which {
                0 => libc::setuid(id),
                1 => libc::setgid(id),
                2 => libc::seteuid(id),
                _ => libc::setegid(id),
            }
        } == 0;
        if ok {
            rv.set(v8::null(scope).into());
        } else {
            let name = errno_name(std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
            let msg = v8::String::new(scope, name).expect("errno name is short");
            rv.set(msg.into());
        }
    }
    #[cfg(not(unix))]
    {
        let (_, _) = (which, raw);
        let msg = v8::String::new(scope, "ENOSYS").expect("static errno string");
        rv.set(msg.into());
    }
}

fn op_posix_get_groups(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    #[cfg(unix)]
    {
        let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
        if count < 0 {
            rv.set(v8::null(scope).into());
            return;
        }
        let mut buf = vec![0 as libc::gid_t; count as usize];
        let got = unsafe { libc::getgroups(count, buf.as_mut_ptr()) };
        if got < 0 {
            rv.set(v8::null(scope).into());
            return;
        }
        buf.truncate(got as usize);
        let arr = v8::Array::new(scope, got);
        for (i, gid) in buf.iter().enumerate() {
            let n = v8::Number::new(scope, f64::from(*gid));
            arr.set_index(scope, i as u32, n.into());
        }
        rv.set(arr.into());
    }
    #[cfg(not(unix))]
    {
        rv.set(v8::null(scope).into());
    }
}

/// Takes an array of numeric gids. Returns null on success or an errno name.
fn op_posix_set_groups(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    #[cfg(unix)]
    {
        let Ok(arr) = v8::Local::<v8::Array>::try_from(args.get(0)) else {
            let msg = v8::String::new(scope, "EINVAL").expect("static errno string");
            rv.set(msg.into());
            return;
        };
        let len = arr.length();
        let mut gids: Vec<libc::gid_t> = Vec::with_capacity(len as usize);
        for i in 0..len {
            let Some(v) = arr.get_index(scope, i) else {
                continue;
            };
            gids.push(v.number_value(scope).unwrap_or(0.0) as libc::gid_t);
        }
        let ok = unsafe { libc::setgroups(gids.len() as _, gids.as_ptr()) } == 0;
        if ok {
            rv.set(v8::null(scope).into());
        } else {
            let name = errno_name(std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
            let msg = v8::String::new(scope, name).expect("errno name is short");
            rv.set(msg.into());
        }
    }
    #[cfg(not(unix))]
    {
        let _ = args;
        let msg = v8::String::new(scope, "ENOSYS").expect("static errno string");
        rv.set(msg.into());
    }
}

/// initgroups(user, extraGid). Returns null on success or an errno name.
fn op_posix_init_groups(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    #[cfg(unix)]
    {
        let Some(user) = arg_string(scope, &args, 0) else {
            let msg = v8::String::new(scope, "EINVAL").expect("static errno string");
            rv.set(msg.into());
            return;
        };
        let gid = args.get(1).number_value(scope).unwrap_or(0.0) as libc::gid_t;
        let Ok(cuser) = std::ffi::CString::new(user) else {
            let msg = v8::String::new(scope, "EINVAL").expect("static errno string");
            rv.set(msg.into());
            return;
        };
        // `as _`, not a fixed cast: initgroups(3) takes c_int on macOS and
        // gid_t (u32) on Linux, so a concrete type breaks one of them.
        let ok = unsafe { libc::initgroups(cuser.as_ptr(), gid as _) } == 0;
        if ok {
            rv.set(v8::null(scope).into());
        } else {
            let name = errno_name(std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
            let msg = v8::String::new(scope, name).expect("errno name is short");
            rv.set(msg.into());
        }
    }
    #[cfg(not(unix))]
    {
        let _ = args;
        let msg = v8::String::new(scope, "ENOSYS").expect("static errno string");
        rv.set(msg.into());
    }
}

/// Credential lookup. `kind`: 0 = user NAME -> uid (getpwnam), 1 = group
/// NAME -> gid (getgrnam), 2 = uid -> user NAME (getpwuid, the reverse
/// direction initgroups(3) needs since it takes a name, not an id).
/// Returns null when there is no such entry, which JS turns into Node's
/// ERR_UNKNOWN_CREDENTIAL.
fn op_posix_lookup_id(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let kind = args.get(0).int32_value(scope).unwrap_or(0);
    #[cfg(unix)]
    {
        // uid -> name is the one direction whose input is numeric.
        if kind == 2 {
            let uid = args.get(1).number_value(scope).unwrap_or(-1.0);
            if !(0.0..=f64::from(u32::MAX)).contains(&uid) || uid.fract() != 0.0 {
                rv.set(v8::null(scope).into());
                return;
            }
            // getpwuid returns a pointer into static storage: copy the name
            // out immediately, never retain the pointer.
            let name = unsafe {
                let pw = libc::getpwuid(uid as libc::uid_t);
                if pw.is_null() {
                    None
                } else {
                    std::ffi::CStr::from_ptr((*pw).pw_name)
                        .to_str()
                        .ok()
                        .map(str::to_string)
                }
            };
            match name.and_then(|n| v8::String::new(scope, &n)) {
                Some(s) => rv.set(s.into()),
                None => rv.set(v8::null(scope).into()),
            }
            return;
        }
        let Some(name) = arg_string(scope, &args, 1) else {
            rv.set(v8::null(scope).into());
            return;
        };
        let Ok(cname) = std::ffi::CString::new(name) else {
            rv.set(v8::null(scope).into());
            return;
        };
        // Same static-storage rule as above.
        let id: Option<u32> = unsafe {
            if kind == 0 {
                let pw = libc::getpwnam(cname.as_ptr());
                if pw.is_null() {
                    None
                } else {
                    Some((*pw).pw_uid)
                }
            } else {
                let gr = libc::getgrnam(cname.as_ptr());
                if gr.is_null() {
                    None
                } else {
                    Some((*gr).gr_gid)
                }
            }
        };
        match id {
            Some(v) => rv.set_double(f64::from(v)),
            None => rv.set(v8::null(scope).into()),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = kind;
        rv.set(v8::null(scope).into());
    }
}

/// Apply a `process.env.TZ` write to the actual time zone. Node intercepts
/// TZ writes and re-reads the zone; without this, assigning TZ changed a
/// string in a JS object and nothing else, so every `Date` kept the zone the
/// process started in. Passing null clears TZ (back to the host zone).
fn op_set_timezone(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let value = arg_string(scope, &args, 0);
    // SAFETY: edition 2024 marks env mutation unsafe because it races other
    // threads calling getenv. This runs on the isolate thread during a JS
    // property set, and the tzset/ICU refresh below is the whole point of
    // the call -- the same trade Node makes for process.env.TZ.
    unsafe {
        match &value {
            Some(tz) => std::env::set_var("TZ", tz),
            None => std::env::remove_var("TZ"),
        }
    }
    // libc caches the parsed zone; tzset() re-reads TZ for anything going
    // through localtime(3). Declared here rather than used from the libc
    // crate, which does not export it on darwin.
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn tzset();
        }
        unsafe { tzset() };
    }
    // V8 caches its own zone for Date: without this notification the change
    // is invisible to JS no matter what the environment says.
    scope.date_time_configuration_change_notification(v8::TimeZoneDetection::Redetect);
}

/// execve(2): replace the current process image. On success this NEVER
/// returns -- the process is gone, which is why nothing after it can run and
/// why 'exit' handlers must not fire. On failure it returns the errno name.
///
/// Args: (path, argv[], envp[] as "K=V"). Validation lives in JS so the
/// error shapes match Node exactly; by here everything is a clean string.
fn op_process_execve(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        let Some(path) = arg_string(scope, &args, 0) else {
            let msg = v8::String::new(scope, "EINVAL").expect("static errno string");
            rv.set(msg.into());
            return;
        };
        // Checked BEFORE anything is handed to execve: on success this call
        // never returns, so a permission consulted afterwards would never be
        // consulted at all. Replacing the image is a child-process operation
        // in node's model -- whatever runs next is outside this permission set
        // entirely.
        if let Err(denial) = get_permissions(scope).check_child(&path) {
            throw_permission_denied(scope, &denial);
            return;
        }
        let to_cstrings = |scope: &mut v8::PinScope<'_, '_>, idx: i32| -> Option<Vec<CString>> {
            let arr = v8::Local::<v8::Array>::try_from(args.get(idx)).ok()?;
            let mut out = Vec::with_capacity(arr.length() as usize);
            for i in 0..arr.length() {
                let v = arr.get_index(scope, i)?;
                let s = v.to_rust_string_lossy(scope);
                out.push(CString::new(s).ok()?);
            }
            Some(out)
        };
        let (Ok(cpath), Some(argv), Some(envp)) = (
            CString::new(path),
            to_cstrings(scope, 1),
            to_cstrings(scope, 2),
        ) else {
            let msg = v8::String::new(scope, "EINVAL").expect("static errno string");
            rv.set(msg.into());
            return;
        };
        // Flush before the image is replaced: anything still sitting in a
        // userspace buffer is lost the instant execve succeeds.
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();

        let mut argv_ptrs: Vec<*const libc::c_char> = argv.iter().map(|c| c.as_ptr()).collect();
        argv_ptrs.push(std::ptr::null());
        let mut envp_ptrs: Vec<*const libc::c_char> = envp.iter().map(|c| c.as_ptr()).collect();
        envp_ptrs.push(std::ptr::null());

        unsafe {
            libc::execve(cpath.as_ptr(), argv_ptrs.as_ptr(), envp_ptrs.as_ptr());
        }
        // Only reachable when execve failed.
        let name = errno_name(std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
        let msg = v8::String::new(scope, name).expect("errno name is short");
        rv.set(msg.into());
    }
    #[cfg(not(unix))]
    {
        let _ = args;
        let msg = v8::String::new(scope, "ENOSYS").expect("static errno string");
        rv.set(msg.into());
    }
}

/// The handful of errno values these ops can realistically produce, by name.
/// Anything else is reported numerically rather than guessed at.
#[cfg(unix)]
fn errno_name(code: i32) -> &'static str {
    match code {
        libc::EPERM => "EPERM",
        libc::EINVAL => "EINVAL",
        libc::ESRCH => "ESRCH",
        libc::ENOSYS => "ENOSYS",
        // exec(2) failure modes -- ENOENT is the one users hit (bad path).
        libc::ENOENT => "ENOENT",
        libc::EACCES => "EACCES",
        libc::ENOEXEC => "ENOEXEC",
        libc::E2BIG => "E2BIG",
        libc::ENOMEM => "ENOMEM",
        libc::ELOOP => "ELOOP",
        libc::ENAMETOOLONG => "ENAMETOOLONG",
        libc::ENOTDIR => "ENOTDIR",
        libc::EISDIR => "EISDIR",
        libc::ETXTBSY => "ETXTBSY",
        _ => "EIO",
    }
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

// --------------------------------------------------------------- os signals
// process.on('SIGTERM'|'SIGINT'|'SIGHUP', fn) delivery. The JS layer gates
// these on listenerCount so startSignal fires for the first listener of a
// signal and stopSignal for the last; start is idempotent core-side too.

fn op_start_signal(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    if let Some(name) = arg_string(scope, &args, 0) {
        core_runtime_mut!(scope).start_signal(&name);
    }
}

fn op_stop_signal(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    if let Some(name) = arg_string(scope, &args, 0) {
        core_runtime_mut!(scope).stop_signal(&name);
    }
}

// ------------------------------------------------------------ http server

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
    let rt = core_runtime!(scope);
    let state = rt.http();
    let tcp = rt.tcp();
    let tcp_ids = rt.body_ids();
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::http_server::http_serve(
            state,
            tcp,
            tcp_ids,
            host,
            port,
            // arg 2: opt into dispatch-on-headers + streamed request bodies.
            args.get(2).is_true(),
        ),
    );
}

fn op_http_accept(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let server_id = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let state = core_runtime!(scope).http();
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::http_server::http_accept(state, server_id),
    );
}

/// Read the next chunk of a STREAMED request body. Resolves a Uint8Array,
/// or Done at EOF. Remove-await-reinsert on the receiver, matching the
/// accept queue.
/// Open an outbound request-body channel. Returns the handle to put in the
/// fetch request's `body_stream`. Slice 4 of docs/design/streaming-bodies.md.
fn op_fetch_body_channel_new(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = core_runtime!(scope).new_outbound_body();
    rv.set(v8::Number::new(scope, handle as f64).into());
}

/// Push a chunk. Resolves when the chunk is accepted, so JS write()
/// backpressure follows the socket.
fn op_fetch_body_channel_write(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let Some(bytes) = arg_bytes(scope, &args, 1) else {
        throw_type_error(scope, "fetchBodyChannelWrite requires bytes");
        return;
    };
    let outbound = core_runtime!(scope).outbound_bodies();
    crate::ops::spawn_op(scope, &mut rv, async move {
        let tx = outbound
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&handle)
            .and_then(|slot| slot.0.clone());
        let Some(tx) = tx else {
            return oam_core::OpOutcome::Failed(format!("unknown body stream {handle}"));
        };
        match tx.send(Ok(bytes)).await {
            Ok(()) => oam_core::OpOutcome::Done,
            // Receiver gone: the request finished or failed. Not an error to
            // the writer -- the transport already reported it.
            Err(_) => oam_core::OpOutcome::Done,
        }
    });
}

/// Close the channel: drops the sender, which ends the request body.
fn op_fetch_body_channel_end(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    if let Some(slot) = core_runtime!(scope)
        .outbound_bodies()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(&handle)
    {
        slot.0 = None;
    }
}

/// Abort an in-flight upload (req.destroy()): send an error so the transport
/// tears the request down instead of completing it, then drop the entry.
fn op_fetch_body_channel_cancel(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let entry = core_runtime!(scope)
        .outbound_bodies()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&handle);
    if let Some((Some(tx), _)) = entry {
        let _ = tx.try_send(Err("request aborted".to_string()));
    }
}

fn op_http_request_body_read(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let id = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let state = core_runtime!(scope).http();
    crate::ops::spawn_op(scope, &mut rv, async move {
        let mut rx = match state.take_body_stream(id) {
            oam_core::http_server::BodyCheckout::Ready(rx) => rx,
            // Checked out by another read: reporting EOF here is what made
            // the server see an empty body mid-stream. Surface it instead.
            oam_core::http_server::BodyCheckout::InFlight => {
                return oam_core::OpOutcome::Failed(
                    "concurrent read of the same request body".to_string(),
                );
            }
            oam_core::http_server::BodyCheckout::Absent => {
                // Not a streamed body: a buffered one (TLS and http2 still
                // collect, and so does any server that did not opt in). Hand it
                // over as a single chunk; the take empties the registry so the
                // next read reports EOF.
                return match state.take_request_body(id) {
                    Some(bytes) if !bytes.is_empty() => oam_core::OpOutcome::Bytes(bytes),
                    _ => oam_core::OpOutcome::Done,
                };
            }
        };
        let next = rx.recv().await;
        // Only reinsert while the stream is live. EOF/error must also REMOVE
        // the StreamPending marker left by the checkout: once a response is
        // in flight the engine-side guard no longer reaps this entry (a
        // full-duplex handler may still be reading), so a finished stream
        // left as a marker would leak until process exit.
        match next {
            Some(Ok(chunk)) => {
                state.put_body_stream(id, rx);
                // into_data releases the chunk's budget reservation.
                oam_core::OpOutcome::Bytes(chunk.into_data())
            }
            Some(Err(e)) => {
                state.cancel_body_stream(id);
                oam_core::OpOutcome::Failed(e)
            }
            None => {
                state.cancel_body_stream(id);
                oam_core::OpOutcome::Done
            }
        }
    });
}

/// Drop a streamed request body (req.destroy() / handler done early). Stops
/// the pump, which stops reading the socket.
fn op_http_request_body_cancel(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let id = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    core_runtime!(scope).http().cancel_body_stream(id);
}

fn op_http_request_body(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let id = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let bytes = core_runtime!(scope)
        .http()
        .take_request_body(id)
        .unwrap_or_default();
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
    rv.set_bool(
        core_runtime!(scope)
            .http()
            .respond_full(id, status, headers, body),
    );
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
    // Exchange gone (aborted via httpAbort, or already answered) leaves rv
    // undefined, NOT a throw -- Node's post-abort res.write() is a soft
    // failure, and the JS layer maps this to a premature close.
    if let Some(stream_id) = core_runtime!(scope)
        .http()
        .respond_stream(id, status, headers)
    {
        rv.set_double(stream_id as f64);
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
    let state = core_runtime!(scope).http();
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
    core_runtime!(scope).http().end_stream(stream_id);
}

fn op_http_stream_closed(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let stream_id = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let state = core_runtime!(scope).http();
    // UNREF'd: this is a passive watcher for "the client went away", which
    // may never happen. hyper only notices a vanished peer when it next
    // tries to write, so a server that stops writing would otherwise leave
    // this op pending forever and pin the event loop open.
    crate::ops::spawn_op_unref(
        scope,
        &mut rv,
        oam_core::http_server::http_stream_closed(state, stream_id),
    );
}

fn op_http_abort(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let id = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    rv.set_bool(core_runtime!(scope).http().abort_request(id));
}

fn op_http_close(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let server_id = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    core_runtime!(scope).http().close_server(server_id);
}

// ----------------------------------------------------------------- HTTP/2

fn op_http2_serve(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let host = arg_string(scope, &args, 0).unwrap_or_else(|| "127.0.0.1".to_string());
    let port = args.get(1).number_value(scope).unwrap_or(0.0) as u16;
    let net_resource = format!("{host}:{port}");
    if !check_net_perm(scope, &net_resource) {
        return;
    }
    let state = core_runtime!(scope).http();
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::http_server::http2_serve(state, host, port),
    );
}

// ----------------------------------------------------------------- HTTPS

fn op_https_serve(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let host = arg_string(scope, &args, 0).unwrap_or_else(|| "127.0.0.1".to_string());
    let port = args.get(1).number_value(scope).unwrap_or(0.0) as u16;
    let Some(cert_pem) = arg_string(scope, &args, 2) else {
        throw_type_error(scope, "httpsServe requires cert PEM");
        return;
    };
    let Some(key_pem) = arg_string(scope, &args, 3) else {
        throw_type_error(scope, "httpsServe requires key PEM");
        return;
    };
    let net_resource = format!("{host}:{port}");
    if !check_net_perm(scope, &net_resource) {
        return;
    }
    let state = core_runtime!(scope).http();
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::http_server::https_serve(state, host, port, cert_pem, key_pem),
    );
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
    let core = core_runtime!(scope);
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
    let tcp = core_runtime!(scope).tcp();
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
    let tcp = core_runtime!(scope).tcp();
    crate::ops::spawn_op(scope, &mut rv, oam_core::tcp::tcp_write(tcp, handle, data));
}

fn op_tcp_close(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let tcp = core_runtime!(scope).tcp();
    oam_core::tcp::tcp_close(&tcp, handle);
}

fn op_tcp_shutdown(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let tcp = core_runtime!(scope).tcp();
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
    let core = core_runtime!(scope);
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
    let core = core_runtime!(scope);
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
    let tcp = core_runtime!(scope).tcp();
    oam_core::tcp::tcp_server_close(&tcp, server_id);
}

// ------------------------------------------------------------------- UDP

fn op_udp_bind(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(host) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "udpBind requires a host");
        return;
    };
    let port = args.get(1).number_value(scope).unwrap_or(0.0) as u16;
    let net_resource = format!("{host}:{port}");
    if !check_net_perm(scope, &net_resource) {
        return;
    }
    let core = core_runtime!(scope);
    let udp = core.udp();
    let ids = core.body_ids();
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::udp::udp_bind(udp, ids, host, port),
    );
}

fn op_udp_send(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let Some(data) = arg_bytes(scope, &args, 1) else {
        throw_type_error(scope, "udpSend requires data");
        return;
    };
    let Some(host) = arg_string(scope, &args, 2) else {
        throw_type_error(scope, "udpSend requires a target host");
        return;
    };
    let port = args.get(3).number_value(scope).unwrap_or(0.0) as u16;
    let udp = core_runtime!(scope).udp();
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::udp::udp_send(udp, handle, data, host, port),
    );
}

fn op_udp_recv(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let len = args.get(1).number_value(scope).unwrap_or(65536.0) as usize;
    let udp = core_runtime!(scope).udp();
    crate::ops::spawn_op(scope, &mut rv, oam_core::udp::udp_recv(udp, handle, len));
}

fn op_udp_close(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let udp = core_runtime!(scope).udp();
    oam_core::udp::udp_close(&udp, handle);
}

// ------------------------------------------------------------------- TLS

fn op_tls_connect(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(host) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "tlsConnect requires a host");
        return;
    };
    let port = args.get(1).number_value(scope).unwrap_or(0.0) as u16;
    let server_name = arg_string(scope, &args, 2).filter(|s| !s.is_empty());
    let ca_pem = arg_string(scope, &args, 3).filter(|s| !s.is_empty());
    let reject_unauthorized = args.get(4).boolean_value(scope);
    // Empty PEM strings mean "not provided" -- only the (Some, Some) cert+key
    // pair should build a client-auth config (build_client_config:103);
    // otherwise a server-auth-only connection wrongly enters that branch.
    let client_cert_pem = arg_string(scope, &args, 5).filter(|s| !s.is_empty());
    let client_key_pem = arg_string(scope, &args, 6).filter(|s| !s.is_empty());
    let net_resource = format!("{host}:{port}");
    if !check_net_perm(scope, &net_resource) {
        return;
    }
    let core = core_runtime!(scope);
    let tls = core.tls();
    let ids = core.body_ids();
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::tls::tls_connect(
            tls,
            ids,
            host,
            port,
            server_name,
            ca_pem,
            reject_unauthorized,
            client_cert_pem,
            client_key_pem,
        ),
    );
}

fn op_tls_read(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let len = args.get(1).number_value(scope).unwrap_or(65536.0) as usize;
    let tls = core_runtime!(scope).tls();
    crate::ops::spawn_op(scope, &mut rv, oam_core::tls::tls_read(tls, handle, len));
}

fn op_tls_write(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let Some(data) = arg_bytes(scope, &args, 1) else {
        throw_type_error(scope, "tlsWrite requires data");
        return;
    };
    let tls = core_runtime!(scope).tls();
    crate::ops::spawn_op(scope, &mut rv, oam_core::tls::tls_write(tls, handle, data));
}

fn op_tls_close(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let tls = core_runtime!(scope).tls();
    oam_core::tls::tls_close(&tls, handle);
}

fn op_tls_shutdown(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let tls = core_runtime!(scope).tls();
    crate::ops::spawn_op(scope, &mut rv, oam_core::tls::tls_shutdown(tls, handle));
}

fn op_tls_accept_wrap(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let tcp_handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let Some(cert_pem) = arg_string(scope, &args, 1) else {
        throw_type_error(scope, "tlsAcceptWrap requires cert PEM");
        return;
    };
    let Some(key_pem) = arg_string(scope, &args, 2) else {
        throw_type_error(scope, "tlsAcceptWrap requires key PEM");
        return;
    };
    let core = core_runtime!(scope);
    let tls = core.tls();
    let tcp = core.tcp();
    let ids = core.body_ids();
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::tls::tls_accept_wrap(tls, tcp, ids, tcp_handle, cert_pem, key_pem),
    );
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
    let core = core_runtime!(scope);
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
    let streams = core_runtime!(scope).zlib_streams();
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
    let streams = core_runtime!(scope).zlib_streams();
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
    let streams = core_runtime!(scope).zlib_streams();
    oam_core::ops::zlib_stream_close(&streams, handle);
}

/// zlibHandleCreate(mode, level) -> handle (number).
/// Allocates a low-level flate2 Compress/Decompress for Node's internal
/// zlib binding interface (ssh2's ZlibHandle pattern).
fn op_zlib_handle_create(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let mode = args.get(0).int32_value(scope).unwrap_or(0);
    let level = args.get(1).int32_value(scope).unwrap_or(-1);
    let core = core_runtime!(scope);
    let streams = core.zlib_streams();
    let ids = core.body_ids();
    match oam_core::ops::zlib_handle_create(&streams, &ids, mode, level) {
        Ok(handle) => {
            let val = v8::Number::new(scope, handle as f64);
            rv.set(val.into());
        }
        Err(e) => {
            throw_type_error(scope, &e);
        }
    }
}

/// zlibHandleWriteSync(handle, flush, input, outBuf, outOff, outLen)
///   -> [availOutAfter, availInAfter].
/// Synchronous incremental compress/decompress. Writes output directly
/// into outBuf's backing store at outOff.
fn op_zlib_handle_write_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let flush = args.get(1).int32_value(scope).unwrap_or(0);
    let input = arg_bytes(scope, &args, 2).unwrap_or_default();
    let out_off = args.get(4).int32_value(scope).unwrap_or(0) as usize;
    let out_len = args.get(5).int32_value(scope).unwrap_or(0) as usize;

    let streams = core_runtime!(scope).zlib_streams();

    let mut output = vec![0u8; out_len];
    let result =
        oam_core::ops::zlib_handle_write_sync(&streams, handle, flush, &input, &mut output);

    match result {
        Ok((avail_out, avail_in)) => {
            let produced = out_len - avail_out;
            if produced > 0 {
                let out_buf_value = args.get(3);
                if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(out_buf_value)
                    && let Some(buf) = view.buffer(scope)
                {
                    let store = buf.get_backing_store();
                    if let Some(data) = store.data() {
                        let byte_offset = view.byte_offset() + out_off;
                        if byte_offset + produced <= store.byte_length() {
                            unsafe {
                                let dest = (data.as_ptr() as *mut u8).add(byte_offset);
                                std::ptr::copy_nonoverlapping(output.as_ptr(), dest, produced);
                            }
                        } else {
                            let message = v8::String::new(
                                    scope,
                                    "zlib: output buffer overflow (offset + produced exceeds buffer length)",
                                ).unwrap();
                            let exception = v8::Exception::range_error(scope, message);
                            scope.throw_exception(exception);
                            return;
                        }
                    }
                }
            }
            let ao = v8::Number::new(scope, avail_out as f64);
            let ai = v8::Number::new(scope, avail_in as f64);
            let arr = v8::Array::new(scope, 2);
            arr.set_index(scope, 0, ao.into());
            arr.set_index(scope, 1, ai.into());
            rv.set(arr.into());
        }
        Err(e) => {
            let message = v8::String::new(scope, &e).unwrap();
            let exception = v8::Exception::error(scope, message);
            scope.throw_exception(exception);
        }
    }
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

// V8's Object::GetConstructorName reads the constructor from the object's MAP,
// which survives Object.setPrototypeOf(obj, null) -- so util.inspect can label a
// null-prototype object "[Foo: null prototype]" the way Node does (impossible
// from pure JS once the prototype chain is gone). Returns "" for non-objects.
fn op_get_constructor_name(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let value = args.get(0);
    if let Ok(obj) = v8::Local::<v8::Object>::try_from(value) {
        let name = obj.get_constructor_name();
        rv.set(name.into());
    } else {
        let empty = v8::String::empty(scope);
        rv.set(empty.into());
    }
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
    if !check_read_perm(scope, &path) {
        return;
    }
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
    match oam_core::ops::stat_path_json(&path, lstat) {
        Ok(json) => return_json(scope, &mut rv, &json),
        Err(e) => throw_node_error(scope, if lstat { "lstat" } else { "stat" }, &path, &e),
    }
}

fn op_fs_statfs_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "statfsSync requires a path");
        return;
    };
    if !check_read_perm(scope, &path) {
        return;
    }
    match oam_core::ops::statfs_json(&path) {
        Ok(json) => return_json(scope, &mut rv, &json),
        Err(e) => throw_node_error(scope, "statfs", &path, &e),
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
    if !check_write_perm(scope, &path) {
        return;
    }
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
    if !check_write_perm(scope, &path) {
        return;
    }
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
    if !check_write_perm(scope, &from) {
        return;
    }
    if !check_write_perm(scope, &to) {
        return;
    }
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
    if !check_read_perm(scope, &from) {
        return;
    }
    if !check_write_perm(scope, &to) {
        return;
    }
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
    if !check_write_perm(scope, &path) {
        return;
    }
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
    if !check_read_perm(scope, &path) {
        return;
    }
    let mode = args.get(1).int32_value(scope).unwrap_or(0);
    match oam_core::check_access(&path, mode) {
        Ok(()) => {}
        Err((code, message, errno)) => {
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
                // errno completes node's system-error shape; it was the one
                // field this hand-rolled error left off.
                if let Some(errno) = errno
                    && let Some(key) = v8::String::new(scope, "errno")
                {
                    let value = v8::Integer::new(scope, errno);
                    obj.set(scope, key.into(), value.into());
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
    if !check_read_perm(scope, &path) {
        return;
    }
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
    if !check_read_perm(scope, &path) {
        return;
    }
    let lstat = args.get(1).is_true();
    crate::ops::spawn_op(scope, &mut rv, oam_core::ops::fs_stat(path, lstat));
}

fn op_fs_fstat(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let fd = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let files = core_runtime!(scope).sync_files();
    // Same adoption rule as the sync twin: a low fd we never allocated is the
    // parent's, passed in at spawn.
    oam_core::adopt_inherited_fd(&files, fd);
    // Clone the handle under the lock and drop the lock immediately -- the
    // metadata read then happens on a blocking thread with nothing held, so a
    // slow device cannot stall every other fd op.
    let cloned = {
        let guard = files.lock().unwrap_or_else(|e| e.into_inner());
        match guard.files.get(&fd) {
            None => {
                throw_ebadf(scope, "fstat");
                return;
            }
            Some(file) => match file.try_clone() {
                Ok(cloned) => cloned,
                Err(e) => {
                    throw_fd_error(scope, "fstat", &e);
                    return;
                }
            },
        }
    };
    crate::ops::spawn_op(scope, &mut rv, oam_core::ops::fs_fstat(cloned));
}

// --------------------------------------------------------- fd-based fs ops
//
// fsync / fdatasync / ftruncate / fchmod / fchown / futimes, async and sync.
// All exempt-by-capability in `permission_audit`: the fd can only have come
// from an `open` that was checked, and there is no path here to re-check.

/// Resolve an fd to an owned `File` for an op that will hand it to a blocking
/// thread. Throws EBADF and returns None when the fd is not open.
///
/// The registry comes in as a parameter rather than from `core_runtime!`: that
/// macro expands to a bare `return;`, which only compiles inside a function
/// returning `()`. The op callbacks do; this does not.
fn clone_registered_fd(
    scope: &mut v8::PinScope<'_, '_>,
    files: &oam_core::SyncFileRegistry,
    fd: u64,
    syscall: &str,
) -> Option<std::fs::File> {
    // Same adoption rule as fstat: a low fd we never allocated came from the
    // parent at spawn.
    oam_core::adopt_inherited_fd(files, fd);
    let cloned = {
        let guard = files.lock().unwrap_or_else(|e| e.into_inner());
        guard.files.get(&fd).map(|file| file.try_clone())
    };
    match cloned {
        None => {
            throw_ebadf(scope, syscall);
            None
        }
        Some(Err(e)) => {
            throw_node_error(scope, syscall, "", &e);
            None
        }
        Some(Ok(file)) => Some(file),
    }
}

/// Run a synchronous fd operation against the registry, throwing on failure.
fn with_registered_fd<F>(scope: &mut v8::PinScope<'_, '_>, fd: u64, syscall: &str, action: F)
where
    F: FnOnce(&std::fs::File) -> std::io::Result<()>,
{
    let files = core_runtime!(scope).sync_files();
    oam_core::adopt_inherited_fd(&files, fd);
    let outcome = {
        let guard = files.lock().unwrap_or_else(|e| e.into_inner());
        guard.files.get(&fd).map(action)
    };
    match outcome {
        None => throw_ebadf(scope, syscall),
        Some(Err(e)) => throw_node_error(scope, syscall, "", &e),
        Some(Ok(())) => {}
    }
}

/// atime/mtime as milliseconds since the epoch. The JS layer already turned
/// numbers, Dates and numeric strings into plain numbers.
fn utime_args(
    scope: &mut v8::PinScope<'_, '_>,
    args: &v8::FunctionCallbackArguments<'_>,
    first: i32,
) -> (f64, f64) {
    let atime = args.get(first).number_value(scope).unwrap_or(0.0);
    let mtime = args.get(first + 1).number_value(scope).unwrap_or(0.0);
    (atime, mtime)
}

fn op_fs_fsync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let fd = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let files = core_runtime!(scope).sync_files();
    let Some(file) = clone_registered_fd(scope, &files, fd, "fsync") else {
        return;
    };
    crate::ops::spawn_op(scope, &mut rv, oam_core::ops::fs_fsync(file, false));
}

fn op_fs_fdatasync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let fd = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let files = core_runtime!(scope).sync_files();
    let Some(file) = clone_registered_fd(scope, &files, fd, "fdatasync") else {
        return;
    };
    crate::ops::spawn_op(scope, &mut rv, oam_core::ops::fs_fsync(file, true));
}

fn op_fs_ftruncate(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let fd = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let len = args.get(1).integer_value(scope).unwrap_or(0).max(0) as u64;
    let files = core_runtime!(scope).sync_files();
    let Some(file) = clone_registered_fd(scope, &files, fd, "ftruncate") else {
        return;
    };
    crate::ops::spawn_op(scope, &mut rv, oam_core::ops::fs_ftruncate(file, len));
}

fn op_fs_fchmod(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let fd = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let mode = args.get(1).uint32_value(scope).unwrap_or(0o644);
    let files = core_runtime!(scope).sync_files();
    let Some(file) = clone_registered_fd(scope, &files, fd, "fchmod") else {
        return;
    };
    crate::ops::spawn_op(scope, &mut rv, oam_core::ops::fs_fchmod(file, mode));
}

fn op_fs_fchown(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let fd = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let uid = args.get(1).uint32_value(scope).unwrap_or(0);
    let gid = args.get(2).uint32_value(scope).unwrap_or(0);
    let files = core_runtime!(scope).sync_files();
    let Some(file) = clone_registered_fd(scope, &files, fd, "fchown") else {
        return;
    };
    crate::ops::spawn_op(scope, &mut rv, oam_core::ops::fs_fchown(file, uid, gid));
}

fn op_fs_futimes(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let fd = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let (atime, mtime) = utime_args(scope, &args, 1);
    let files = core_runtime!(scope).sync_files();
    let Some(file) = clone_registered_fd(scope, &files, fd, "futime") else {
        return;
    };
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::ops::fs_futimes(file, atime, mtime),
    );
}

fn op_fs_fsync_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let fd = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    with_registered_fd(scope, fd, "fsync", |file| file.sync_all());
}

fn op_fs_fdatasync_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let fd = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    with_registered_fd(scope, fd, "fdatasync", |file| file.sync_data());
}

fn op_fs_ftruncate_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let fd = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let len = args.get(1).integer_value(scope).unwrap_or(0).max(0) as u64;
    with_registered_fd(scope, fd, "ftruncate", |file| file.set_len(len));
}

fn op_fs_fchmod_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let fd = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let mode = args.get(1).uint32_value(scope).unwrap_or(0o644);
    with_registered_fd(scope, fd, "fchmod", |file| {
        oam_core::ops::fs_fchmod_sync(file, mode)
    });
}

fn op_fs_fchown_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let fd = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let uid = args.get(1).uint32_value(scope).unwrap_or(0);
    let gid = args.get(2).uint32_value(scope).unwrap_or(0);
    with_registered_fd(scope, fd, "fchown", |file| {
        oam_core::ops::fs_fchown_sync(file, uid, gid)
    });
}

fn op_fs_futimes_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let fd = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let (atime, mtime) = utime_args(scope, &args, 1);
    with_registered_fd(scope, fd, "futime", |file| {
        oam_core::ops::fs_futimes_sync(file, atime, mtime)
    });
}

// ------------------------------------------- path-based ownership / time ops
//
// chown / lchown / utimes / lutimes / lchmod. These NAME a path, so unlike the
// fd family they are not exempt-by-capability -- every one takes a write check.
// `lchown` and `lutimes` act on the link itself, which is the whole point of
// their existence, so the check is on the link path as written.

/// uid/gid pair. node passes -1 through unsigned to mean "leave unchanged".
fn chown_args(
    scope: &mut v8::PinScope<'_, '_>,
    args: &v8::FunctionCallbackArguments<'_>,
) -> (u32, u32) {
    let uid = args.get(1).uint32_value(scope).unwrap_or(u32::MAX);
    let gid = args.get(2).uint32_value(scope).unwrap_or(u32::MAX);
    (uid, gid)
}

fn op_fs_chown(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "chown requires a path");
        return;
    };
    if !check_write_perm(scope, &path) {
        return;
    }
    let (uid, gid) = chown_args(scope, &args);
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::ops::fs_chown(path, uid, gid, true),
    );
}

fn op_fs_lchown(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "lchown requires a path");
        return;
    };
    if !check_write_perm(scope, &path) {
        return;
    }
    let (uid, gid) = chown_args(scope, &args);
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::ops::fs_chown(path, uid, gid, false),
    );
}

fn op_fs_utimes(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "utimes requires a path");
        return;
    };
    if !check_write_perm(scope, &path) {
        return;
    }
    let (atime, mtime) = utime_args(scope, &args, 1);
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::ops::fs_utimes(path, atime, mtime, true),
    );
}

fn op_fs_lutimes(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "lutimes requires a path");
        return;
    };
    if !check_write_perm(scope, &path) {
        return;
    }
    let (atime, mtime) = utime_args(scope, &args, 1);
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::ops::fs_utimes(path, atime, mtime, false),
    );
}

fn op_fs_lchmod(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "lchmod requires a path");
        return;
    };
    if !check_write_perm(scope, &path) {
        return;
    }
    let mode = args.get(1).uint32_value(scope).unwrap_or(0o644);
    crate::ops::spawn_op(scope, &mut rv, oam_core::ops::fs_lchmod(path, mode));
}

fn op_fs_chown_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "chownSync requires a path");
        return;
    };
    if !check_write_perm(scope, &path) {
        return;
    }
    let (uid, gid) = chown_args(scope, &args);
    if let Err(e) = oam_core::ops::fs_chown_sync(&path, uid, gid, true) {
        throw_node_error(scope, "chown", &path, &e);
    }
}

fn op_fs_lchown_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "lchownSync requires a path");
        return;
    };
    if !check_write_perm(scope, &path) {
        return;
    }
    let (uid, gid) = chown_args(scope, &args);
    if let Err(e) = oam_core::ops::fs_chown_sync(&path, uid, gid, false) {
        throw_node_error(scope, "lchown", &path, &e);
    }
}

fn op_fs_utimes_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "utimesSync requires a path");
        return;
    };
    if !check_write_perm(scope, &path) {
        return;
    }
    let (atime, mtime) = utime_args(scope, &args, 1);
    // node reports the SINGULAR `utime` here, not `utimes`.
    if let Err(e) = oam_core::ops::fs_utimes_sync(&path, atime, mtime, true) {
        throw_node_error(scope, "utime", &path, &e);
    }
}

fn op_fs_lutimes_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "lutimesSync requires a path");
        return;
    };
    if !check_write_perm(scope, &path) {
        return;
    }
    let (atime, mtime) = utime_args(scope, &args, 1);
    if let Err(e) = oam_core::ops::fs_utimes_sync(&path, atime, mtime, false) {
        throw_node_error(scope, "lutime", &path, &e);
    }
}

fn op_fs_lchmod_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "lchmodSync requires a path");
        return;
    };
    if !check_write_perm(scope, &path) {
        return;
    }
    let mode = args.get(1).uint32_value(scope).unwrap_or(0o644);
    if let Err(e) = oam_core::ops::fs_lchmod_sync(&path, mode) {
        throw_node_error(scope, "lchmod", &path, &e);
    }
}

fn op_fs_statfs(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "statfs requires a path");
        return;
    };
    if !check_read_perm(scope, &path) {
        return;
    }
    crate::ops::spawn_op(scope, &mut rv, oam_core::ops::fs_statfs(path));
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
    if !check_read_perm(scope, &path) {
        return;
    }
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
    if !check_write_perm(scope, &path) {
        return;
    }
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
    if !check_write_perm(scope, &path) {
        return;
    }
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
    if !check_write_perm(scope, &from) {
        return;
    }
    if !check_write_perm(scope, &to) {
        return;
    }
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
    if !check_read_perm(scope, &from) {
        return;
    }
    if !check_write_perm(scope, &to) {
        return;
    }
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
    if !check_write_perm(scope, &path) {
        return;
    }
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
    if !check_read_perm(scope, &path) {
        return;
    }
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
    if !check_read_perm(scope, &path) {
        return;
    }
    crate::ops::spawn_op(scope, &mut rv, oam_core::ops::fs_realpath(path));
}

fn op_fs_mkdtemp(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let prefix = arg_string(scope, &args, 0).unwrap_or_default();
    // As in the sync twin: check the resolved target, not the prefix -- and
    // hand that same resolved path to the op so the checked path IS the
    // created path (resolving twice would mint two different timestamps).
    let dir = oam_core::ops::mkdtemp_target(&prefix);
    if !check_write_perm(scope, &dir.to_string_lossy()) {
        return;
    }
    crate::ops::spawn_op(scope, &mut rv, oam_core::ops::fs_mkdtemp(dir, prefix));
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
    if !check_write_perm(scope, &path) {
        return;
    }
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
    if !check_read_perm(scope, &path) {
        return;
    }
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
    if !check_read_perm(scope, &existing) {
        return;
    }
    if !check_write_perm(scope, &new_path) {
        return;
    }
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
    if !check_write_perm(scope, &path) {
        return;
    }
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
    if !check_write_perm(scope, &path) {
        return;
    }
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
    if !check_write_perm(scope, &path) {
        return;
    }
    #[cfg(windows)]
    let result = {
        let is_dir = std::fs::metadata(&target)
            .map(|m| m.is_dir())
            .unwrap_or(false);
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
    if !check_read_perm(scope, &path) {
        return;
    }
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
    if !check_read_perm(scope, &existing) {
        return;
    }
    if !check_write_perm(scope, &new_path) {
        return;
    }
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
    if !check_write_perm(scope, &path) {
        return;
    }
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
    if !check_write_perm(scope, &path) {
        return;
    }
    let len = args.get(1).integer_value(scope).unwrap_or(0).max(0) as u64;
    // Which syscall actually failed, as node reports it: open + ftruncate,
    // so a missing path is `open` (see the async twin in oam_core).
    match std::fs::OpenOptions::new().write(true).open(&path) {
        Ok(f) => {
            if let Err(e) = f.set_len(len) {
                throw_node_error(scope, "ftruncate", &path, &e);
            }
        }
        Err(e) => throw_node_error(scope, "open", &path, &e),
    }
}

fn op_fs_mkdtemp_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let prefix = arg_string(scope, &args, 0).unwrap_or_default();
    // Gate the directory that is actually created, not the prefix: with a
    // relative prefix the two are different paths (the prefix resolves under
    // the system temp dir), so checking the prefix denied writes inside a
    // correctly-granted temp dir.
    let dir = oam_core::ops::mkdtemp_target(&prefix);
    if !check_write_perm(scope, &dir.to_string_lossy()) {
        return;
    }
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
    let core = core_runtime!(scope);
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
    // Optional third arg: a read POSITION. Absent/null means "from the
    // cursor", which is what the stream readers pass.
    let position = optional_position(scope, &args, 2);
    let files = core_runtime!(scope).files();
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::ops::fs_read_chunk(files, handle, len, position),
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
    // Optional third arg: a write POSITION (pwrite). Absent/null appends at the
    // cursor, which is what the stream writers pass.
    let position = optional_position(scope, &args, 2);
    let files = core_runtime!(scope).files();
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::ops::fs_write_chunk(files, handle, bytes, position),
    );
}

/// Synchronous: dropping the File closes it (flush happens in write ops).
fn op_fs_close(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let handle = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let files = core_runtime!(scope).files();
    let mut guard = files.lock().unwrap_or_else(|e| e.into_inner());
    // If the File is in flight (removed by a chunk op for its IO await),
    // it is absent here -- record the close so the op's reinsert drops it
    // instead of resurrecting a leaked fd (destroy()-during-read race).
    if guard.files.remove(&handle).is_none() {
        guard.closed.insert(handle);
    }
}

/// Throw an `Error` carrying `.code`/`.syscall` for a bad/missing fd. Node
/// uses EBADF; mirrors throw_node_error's object shape without an io::Error.
fn throw_ebadf(scope: &mut v8::PinScope<'_, '_>, syscall: &str) {
    let msg = format!("EBADF: bad file descriptor, {syscall}");
    let msg_v8 =
        v8::String::new(scope, &msg).unwrap_or_else(|| v8::String::new(scope, "EBADF").unwrap());
    let exception = v8::Exception::error(scope, msg_v8);
    if let Ok(obj) = v8::Local::<v8::Object>::try_from(exception) {
        // errno FIRST -- `Object.keys(err)` is observable and node enumerates
        // errno, code, syscall. See the same note in throw_node_error.
        // EBADF: libuv-win -4083, -EBADF (-9) on Unix.
        if let Some(key) = v8::String::new(scope, "errno") {
            let errno = if cfg!(windows) { -4083 } else { -9 };
            let val = v8::Integer::new(scope, errno);
            obj.set(scope, key.into(), val.into());
        }
        for (name, value) in [("code", "EBADF"), ("syscall", syscall)] {
            let key = v8::String::new(scope, name).unwrap();
            if let Some(v) = v8::String::new(scope, value) {
                obj.set(scope, key.into(), v.into());
            }
        }
    }
    scope.throw_exception(exception);
}

/// fopen-style flag string -> OpenOptions. Re-exported from oam_core so the
/// sync and async opens cannot disagree about what a flag means -- they share
/// one descriptor space, so they have to share one flag table too.
use oam_core::open_options_for;

/// fsOpenSync(path, flags) -> fd. Synchronous open backed by std::fs::File in
/// the sync-file registry; the returned number is the integer fd. The classic
/// callback/`*Sync` fd API some packages (graceful-fs/playwright lock probe)
/// require -- they read `err.code === "ENOENT"` to distinguish missing from
/// locked, so a faithful error code is the whole point.
fn op_fs_open_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "openSync requires a path");
        return;
    };
    let flags = arg_string(scope, &args, 1).unwrap_or_else(|| "r".to_string());
    let is_write =
        flags.contains('w') || flags.contains('a') || flags.contains('+') || flags.contains('x');
    if is_write {
        if !check_write_perm(scope, &path) {
            return;
        }
    } else if !check_read_perm(scope, &path) {
        return;
    }
    match open_options_for(&flags).open(&path) {
        Ok(file) => {
            let core = core_runtime!(scope);
            let id = core
                .body_ids()
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            core.sync_files()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .files
                .insert(id, file);
            let val = v8::Number::new(scope, id as f64);
            rv.set(val.into());
        }
        Err(e) => throw_node_error(scope, "open", &path, &e),
    }
}

/// fsReadSync(fd, buffer, offset, length, position) -> bytesRead. Reads into
/// buffer's backing store at `offset`; `position` (a number) seeks first,
/// null reads from the current position.
/// Read at an explicit position WITHOUT moving the descriptor's cursor, which
/// is what `pread(2)` does and what node's positional `fs.read` family means.
///
/// This used to seek and leave the cursor there. The bug is silent and nasty:
/// node gives `readSync(fd, b, 0, 3, 10)` then `readSync(fd, b, 0, 3, null)`
/// -> "KLM" then "ABC" (the sequential read still starts at 0), while oam gave
/// "KLM" then "NOP". A program that positionally probes a file and then reads
/// sequentially got the wrong bytes with no error anywhere.
///
/// Save/seek/act/restore rather than a real pread: std has no positional read
/// on the stable cross-platform surface (`FileExt` is per-OS and differs in
/// name between unix `read_at` and windows `seek_read`), and the registry is
/// already serialised behind its mutex here, so nothing else can observe the
/// cursor mid-operation.
fn read_at(
    file: &mut std::fs::File,
    buf: &mut [u8],
    position: Option<u64>,
) -> std::io::Result<usize> {
    use std::io::{Read, Seek, SeekFrom};
    let Some(p) = position else {
        return file.read(buf);
    };
    let saved = file.stream_position();
    file.seek(SeekFrom::Start(p))?;
    let result = file.read(buf);
    if let Ok(prev) = saved {
        let _ = file.seek(SeekFrom::Start(prev));
    }
    result
}

/// Write at an explicit position without moving the cursor -- `pwrite(2)`.
/// Same bug and same reasoning as `read_at`.
///
/// A descriptor opened in APPEND mode ignores the position entirely and always
/// writes at the end; that is the OS's behaviour, node's too, and restoring the
/// cursor afterwards does not change it.
fn write_at(file: &mut std::fs::File, bytes: &[u8], position: Option<u64>) -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom};
    let Some(p) = position else {
        return oam_core::write_all_checked(file, bytes);
    };
    let saved = file.stream_position();
    file.seek(SeekFrom::Start(p))?;
    let result = oam_core::write_all_checked(file, bytes);
    if let Ok(prev) = saved {
        let _ = file.seek(SeekFrom::Start(prev));
    }
    result
}

fn op_fs_read_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let fd = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let offset = args.get(2).number_value(scope).unwrap_or(0.0) as usize;
    let length = args.get(3).number_value(scope).unwrap_or(0.0) as usize;
    let position = args.get(4);
    let pos_seek = if position.is_number() {
        let p = position.number_value(scope).unwrap_or(-1.0);
        if p >= 0.0 { Some(p as u64) } else { None }
    } else {
        None
    };

    let files = core_runtime!(scope).sync_files();
    // A low fd missing from the registry cannot be one oam allocated -- the
    // counter starts above the inheritable window -- so it is the parent's to
    // adopt. This is what lets oam BE the child of an extra-fd spawn.
    oam_core::adopt_inherited_fd(&files, fd);
    let mut tmp = vec![0u8; length];
    let read_result = {
        let mut guard = files.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .files
            .get_mut(&fd)
            .map(|file| read_at(file, &mut tmp, pos_seek))
    };
    match read_result {
        None => throw_ebadf(scope, "read"),
        Some(Err(e)) => throw_fd_error(scope, "read", &e),
        Some(Ok(n)) => {
            if n > 0 {
                let buf_value = args.get(1);
                if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(buf_value)
                    && let Some(buffer) = view.buffer(scope)
                {
                    let store = buffer.get_backing_store();
                    if let Some(data) = store.data() {
                        let byte_offset = view.byte_offset() + offset;
                        if byte_offset + n <= store.byte_length() {
                            unsafe {
                                let dest = (data.as_ptr() as *mut u8).add(byte_offset);
                                std::ptr::copy_nonoverlapping(tmp.as_ptr(), dest, n);
                            }
                        }
                    }
                }
            }
            let val = v8::Number::new(scope, n as f64);
            rv.set(val.into());
        }
    }
}

/// fsWriteSync(fd, data, position?) -> bytesWritten. `data` is bytes (Buffer)
/// or a string (UTF-8). `position` (a number) seeks first; null appends at the
/// current position.
fn op_fs_write_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let fd = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let Some(bytes) = arg_bytes(scope, &args, 1) else {
        throw_type_error(scope, "writeSync requires data");
        return;
    };
    let position = args.get(2);
    let pos_seek = if position.is_number() {
        let p = position.number_value(scope).unwrap_or(-1.0);
        if p >= 0.0 { Some(p as u64) } else { None }
    } else {
        None
    };
    let files = core_runtime!(scope).sync_files();
    // A low fd missing from the registry cannot be one oam allocated -- the
    // counter starts above the inheritable window -- so it is the parent's to
    // adopt. This is what lets oam BE the child of an extra-fd spawn.
    oam_core::adopt_inherited_fd(&files, fd);
    let write_result = {
        let mut guard = files.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .files
            .get_mut(&fd)
            .map(|file| write_at(file, &bytes, pos_seek))
    };
    match write_result {
        None => throw_ebadf(scope, "write"),
        Some(Err(e)) => throw_fd_error(scope, "write", &e),
        Some(Ok(())) => {
            let val = v8::Number::new(scope, bytes.len() as f64);
            rv.set(val.into());
        }
    }
}

/// fsCloseSync(fd). Dropping the File closes it. Throws EBADF if the fd is
/// unknown (Node parity).
fn op_fs_close_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let fd = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let files = core_runtime!(scope).sync_files();
    // A low fd missing from the registry cannot be one oam allocated -- the
    // counter starts above the inheritable window -- so it is the parent's to
    // adopt. This is what lets oam BE the child of an extra-fd spawn.
    oam_core::adopt_inherited_fd(&files, fd);
    let removed = files
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .files
        .remove(&fd)
        .is_some();
    if !removed {
        throw_ebadf(scope, "close");
        return;
    }
    // An adopted fd (below OWN_FD_BASE) is a DUP of the parent's descriptor;
    // dropping it above closed only our copy. Close the original too, or the
    // peer of an inherited pipe never sees EOF -- which is precisely how a CDP
    // child says "no more messages" on fd 4.
    if fd < oam_core::OWN_FD_BASE {
        oam_core::close_inherited_fd(fd);
    }
}

/// fsFstatSync(fd) -> stat json (same shape as statSync). Throws EBADF if the
/// fd is unknown.
fn op_fs_fstat_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let fd = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
    let files = core_runtime!(scope).sync_files();
    // A low fd missing from the registry cannot be one oam allocated -- the
    // counter starts above the inheritable window -- so it is the parent's to
    // adopt. This is what lets oam BE the child of an extra-fd spawn.
    oam_core::adopt_inherited_fd(&files, fd);
    // Held across the whole read: fstat has no path to reopen from, so the
    // extra fields have to come off the descriptor the caller already owns.
    let guard = files.lock().unwrap_or_else(|e| e.into_inner());
    let payload = guard.files.get(&fd).map(|file| {
        file.metadata()
            .map(|meta| oam_core::ops::stat_to_json(&meta, oam_core::ops::StatSource::File(file)))
    });
    match payload {
        None => throw_ebadf(scope, "fstat"),
        Some(Err(e)) => throw_fd_error(scope, "fstat", &e),
        Some(Ok(json)) => return_json(scope, &mut rv, &json),
    }
}

// ------------------------------------------------- permission helpers / query

/// Throw Node's `ERR_ACCESS_DENIED`: message "Access to this API has been
/// restricted", plus the `permission` and `resource` own properties Node
/// attaches (and its fatal reporter prints).
pub(crate) fn throw_permission_denied(
    scope: &mut v8::PinScope<'_, '_>,
    denial: &crate::permissions::PermissionDenial,
) {
    let msg_v8 = v8::String::new(scope, &denial.to_string())
        .unwrap_or_else(|| v8::String::new(scope, "ERR_ACCESS_DENIED").unwrap());
    let exception = v8::Exception::error(scope, msg_v8);
    if let Ok(obj) = v8::Local::<v8::Object>::try_from(exception) {
        for (key, value) in [
            ("code", "ERR_ACCESS_DENIED"),
            ("permission", denial.permission),
            ("resource", denial.resource.as_str()),
        ] {
            if let (Some(k), Some(v)) = (v8::String::new(scope, key), v8::String::new(scope, value))
            {
                obj.set(scope, k.into(), v.into());
            }
        }
    }
    scope.throw_exception(exception);
}

/// The isolate's permission set, for code that needs to HAND IT to a child
/// rather than check it (worker / fork spawn).
pub(crate) fn permissions_of(
    scope: &v8::PinScope<'_, '_>,
) -> std::sync::Arc<crate::permissions::Permissions> {
    get_permissions(scope)
}

fn get_permissions(
    scope: &v8::PinScope<'_, '_>,
) -> std::sync::Arc<crate::permissions::Permissions> {
    scope
        .get_slot::<std::sync::Arc<crate::permissions::Permissions>>()
        .cloned()
        .unwrap_or_default()
}

/// Check `read` permission for `path`; throw and return `false` if denied.
/// Usage:
/// ```ignore
/// if !check_read_perm(scope, &path) { return; }
/// ```
fn check_read_perm(scope: &mut v8::PinScope<'_, '_>, path: &str) -> bool {
    if let Err(denial) = get_permissions(scope).check_read(path) {
        throw_permission_denied(scope, &denial);
        false
    } else {
        true
    }
}

/// Check `write` permission for `path`.
fn check_write_perm(scope: &mut v8::PinScope<'_, '_>, path: &str) -> bool {
    if let Err(denial) = get_permissions(scope).check_write(path) {
        throw_permission_denied(scope, &denial);
        false
    } else {
        true
    }
}

/// Check `worker` permission (starting a worker / forked isolate).
pub(crate) fn check_worker_perm(scope: &mut v8::PinScope<'_, '_>, resource: &str) -> bool {
    if let Err(denial) = get_permissions(scope).check_worker(resource) {
        throw_permission_denied(scope, &denial);
        false
    } else {
        true
    }
}

/// Check `child` permission (spawning or replacing the process).
fn check_child_perm(scope: &mut v8::PinScope<'_, '_>, resource: &str) -> bool {
    if let Err(denial) = get_permissions(scope).check_child(resource) {
        throw_permission_denied(scope, &denial);
        false
    } else {
        true
    }
}

/// Check `net` permission for `host`.
fn check_net_perm(scope: &mut v8::PinScope<'_, '_>, host: &str) -> bool {
    if let Err(denial) = get_permissions(scope).check_net(host) {
        throw_permission_denied(scope, &denial);
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

    // arg 2: JSON options from the JS Worker constructor
    // ({ stdout, stderr, execArgv }).
    let options = arg_string(scope, &args, 2)
        .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok())
        .unwrap_or(serde_json::Value::Null);
    let flag = |key: &str| options.get(key).and_then(serde_json::Value::as_bool) == Some(true);
    let worker_options = crate::worker::WorkerOptions {
        pipe_stdout: flag("stdout"),
        pipe_stderr: flag("stderr"),
        exec_argv: options
            .get("execArgv")
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    // `--` terminates option parsing; it is a separator, not
                    // an option, and Node does not report it in execArgv.
                    .filter(|s| *s != "--")
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
    };

    let path = PathBuf::from(&script_path);
    if !path.is_file() {
        throw_type_error(scope, &format!("worker script not found: {script_path}"));
        return;
    }
    // The worker INHERITS this isolate's permissions (see spawn_worker), so
    // this gates starting one at all rather than what it may then do.
    if !check_worker_perm(scope, &script_path) {
        return;
    }

    let core = core_runtime!(scope);
    let workers = core.workers();

    let worker_id = workers.lock().unwrap_or_else(|e| e.into_inner()).next_id();

    let (parent_to_worker_tx, parent_to_worker_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let (worker_to_parent_tx, worker_to_parent_rx) =
        std::sync::mpsc::channel::<oam_core::worker::WorkerEvent>();

    let thread = crate::worker::spawn_worker(
        path,
        worker_data,
        worker_id,
        parent_to_worker_rx,
        worker_to_parent_tx,
        worker_options,
        get_permissions(scope),
    );

    {
        let mut guard = workers.lock().unwrap_or_else(|e| e.into_inner());
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
    let workers = core_runtime!(scope).workers();
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
    let workers = core_runtime!(scope).workers();
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
    let workers = core_runtime!(scope).workers();
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
    // Spawning hands control to a binary this permission set does not
    // describe, so it is gated in its own right -- without this, --permission
    // was a one-line escape (spawn a shell, do anything).
    if !check_child_perm(scope, &command) {
        return;
    }

    let args_val = args.get(1);
    let mut child_args: Vec<String> = Vec::new();
    if let Ok(arr) = v8::Local::<v8::Array>::try_from(args_val) {
        for i in 0..arr.length() {
            if let Some(v) = arr.get_index(scope, i)
                && let Some(s) = v.to_string(scope)
            {
                child_args.push(s.to_rust_string_lossy(scope));
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
            if let Some(name) = names.get_index(scope, i)
                && let Some(val) = env_obj.get(scope, name)
                && let (Some(k), Some(v)) = (
                    name.to_string(scope).map(|s| s.to_rust_string_lossy(scope)),
                    val.to_string(scope).map(|s| s.to_rust_string_lossy(scope)),
                )
            {
                pairs.push((k, v));
            }
        }
        Some(pairs)
    });

    let stdio = arg_stdio_spec(scope, opts, &core_runtime!(scope).sync_files());

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
        stdio,
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
    // Spawning hands control to a binary this permission set does not
    // describe, so it is gated in its own right -- without this, --permission
    // was a one-line escape (spawn a shell, do anything).
    if !check_child_perm(scope, &command) {
        return;
    }

    let args_val = args.get(1);
    let mut child_args: Vec<String> = Vec::new();
    if let Ok(arr) = v8::Local::<v8::Array>::try_from(args_val) {
        for i in 0..arr.length() {
            if let Some(v) = arr.get_index(scope, i)
                && let Some(s) = v.to_string(scope)
            {
                child_args.push(s.to_rust_string_lossy(scope));
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

    let env_pairs: Option<Vec<(String, String)>> = opts.and_then(|o| {
        let key = v8::String::new(scope, "env")?;
        let val = o.get(scope, key.into())?;
        if val.is_null_or_undefined() {
            return None;
        }
        let env_obj = v8::Local::<v8::Object>::try_from(val).ok()?;
        let names = env_obj.get_own_property_names(scope, Default::default())?;
        let mut pairs = Vec::new();
        for i in 0..names.length() {
            if let Some(name) = names.get_index(scope, i)
                && let Some(val) = env_obj.get(scope, name)
                && let (Some(k), Some(v)) = (
                    name.to_string(scope).map(|s| s.to_rust_string_lossy(scope)),
                    val.to_string(scope).map(|s| s.to_rust_string_lossy(scope)),
                )
            {
                pairs.push((k, v));
            }
        }
        Some(pairs)
    });

    let stdio = arg_stdio_spec(scope, opts, &core_runtime!(scope).sync_files());

    let children = core_runtime!(scope).children();
    let ids = core_runtime!(scope).body_ids();

    // SYNCHRONOUS spawn (node's uv_spawn is synchronous): the child exists and
    // its pid is known before this op returns, so JS can set `child.pid`
    // immediately -- execa, tree-kill and pidusage all read it the instant
    // spawn() returns. The runtime-context guard is required: tokio's
    // Command::spawn registers the child with the signal-driver reactor.
    let spawned = {
        let core = core_runtime!(scope);
        let _guard = core.enter();
        oam_core::child::spawn_child(command, child_args, cwd, env_pairs, shell, clear_env, stdio)
    };
    match spawned {
        Ok((child, pid)) => {
            let handle = ids.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            {
                let mut guard = children.lock().unwrap_or_else(|e| e.into_inner());
                guard.insert(handle, oam_core::child::ChildProcess::new(child, pid));
            }
            let json = serde_json::json!({ "handle": handle, "pid": pid });
            if let Some(s) = v8::String::new(scope, &json.to_string()) {
                rv.set(s.into());
            }
        }
        Err(msg) => {
            // Keep the failure shape callers already parse (a JSON error
            // body), surfaced synchronously as a thrown Error.
            let text = v8::String::new(scope, &msg).unwrap();
            let err = v8::Exception::error(scope, text);
            scope.throw_exception(err);
        }
    }
}

fn op_spawn_kill(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(handle) = arg_child_handle(scope, &args, 0) else {
        throw_type_error(scope, "spawnKill: valid child handle required");
        return;
    };
    let signal = arg_kill_signal(scope, &args, 1);
    let children = core_runtime!(scope).children();
    oam_core::child::child_kill(&children, handle, signal);
}

/// Read the `stdio` option: a 3-element array the JS layer derives from node's
/// `stdio` option, where 0=ignore, 1=inherit, 2=pipe and **anything >= 3 is a
/// DESCRIPTOR NUMBER** -- the `stdio: ['ignore', logFd, logFd]` shape.
///
/// A numbered slot is resolved here rather than in oam_core because only the
/// engine can reach the descriptor registry. Those slots used to collapse to
/// `inherit`, which sent the child's output to the parent's console instead of
/// the file the caller named. An fd that resolves to nothing keeps the old
/// inherit fallback -- it is the same descriptor oam would EBADF on, and a
/// silent inherit is a smaller lie than a dead slot.
///
/// Absent or malformed means node's default, all three piped.
/// `files` is passed in rather than taken from the scope: `core_runtime!`
/// early-returns `()` on failure, so it cannot be used in a function that
/// returns a value.
fn arg_stdio_spec(
    scope: &mut v8::PinScope<'_, '_>,
    opts: Option<v8::Local<'_, v8::Object>>,
    files: &oam_core::SyncFileRegistry,
) -> oam_core::child::StdioSpec {
    let mut spec = oam_core::child::stdio_pipe_all();
    let Some(arr) = opts.and_then(|o| {
        let key = v8::String::new(scope, "stdio")?;
        let val = o.get(scope, key.into())?;
        v8::Local::<v8::Array>::try_from(val).ok()
    }) else {
        return spec;
    };
    for (fd, slot) in spec.iter_mut().enumerate() {
        let Some(code) = arr
            .get_index(scope, fd as u32)
            .and_then(|v| v.number_value(scope))
            .filter(|c| c.is_finite())
        else {
            continue;
        };
        *slot = if code >= 3.0 {
            let target = code as u64;
            // The parent may have handed US this descriptor; adopt it first so
            // a pass-through of an inherited fd resolves like any other.
            oam_core::adopt_inherited_fd(files, target);
            let dup = files
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .files
                .get(&target)
                // A dup, so handing it to the child leaves the caller's fd
                // usable and independently closable.
                .and_then(|f| f.try_clone().ok());
            match dup {
                Some(file) => oam_core::child::StdioMode::File(file),
                None => {
                    eprintln!(
                        "oam: stdio fd {target} did not resolve to an open descriptor; child inherits parent stdio"
                    );
                    oam_core::child::StdioMode::Inherit
                }
            }
        } else {
            oam_core::child::StdioMode::from_code(code as u8)
        };
    }
    spec
}

/// Extract an optional signal-name string argument (e.g. "SIGTERM").
fn arg_kill_signal(
    scope: &mut v8::PinScope<'_, '_>,
    args: &v8::FunctionCallbackArguments<'_>,
    idx: i32,
) -> Option<String> {
    let v = args.get(idx);
    if v.is_string() {
        v.to_string(scope).map(|s| s.to_rust_string_lossy(scope))
    } else {
        None
    }
}

/// Extract a child-registry handle argument. Returns Some only for a finite
/// number >= 0; a non-numeric/undefined/NaN value is None so the caller can
/// reject it instead of silently coercing to handle 0 (a real child).
fn arg_child_handle(
    scope: &mut v8::PinScope<'_, '_>,
    args: &v8::FunctionCallbackArguments<'_>,
    idx: i32,
) -> Option<u64> {
    args.get(idx)
        .number_value(scope)
        .filter(|n| n.is_finite() && *n >= 0.0)
        .map(|n| n as u64)
}

fn op_spawn_read_stdout(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(handle) = arg_child_handle(scope, &args, 0) else {
        throw_type_error(scope, "spawnReadStdout: valid child handle required");
        return;
    };
    let children = core_runtime!(scope).children();
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
    let Some(handle) = arg_child_handle(scope, &args, 0) else {
        throw_type_error(scope, "spawnReadStderr: valid child handle required");
        return;
    };
    let children = core_runtime!(scope).children();
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
    let Some(handle) = arg_child_handle(scope, &args, 0) else {
        throw_type_error(scope, "spawnWrite: valid child handle required");
        return;
    };
    let Some(data) = arg_bytes(scope, &args, 1) else {
        throw_type_error(scope, "spawnWrite: data required");
        return;
    };
    let children = core_runtime!(scope).children();
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::child::child_write_stdin(children, handle, data),
    );
}

fn op_spawn_close_stdin(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(handle) = arg_child_handle(scope, &args, 0) else {
        throw_type_error(scope, "spawnCloseStdin: valid child handle required");
        return;
    };
    let children = core_runtime!(scope).children();
    oam_core::child::child_close_stdin(&children, handle);
}

fn op_spawn_wait(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(handle) = arg_child_handle(scope, &args, 0) else {
        throw_type_error(scope, "spawnWait: valid child handle required");
        return;
    };
    let children = core_runtime!(scope).children();
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::child::child_wait(children, handle),
    );
}

// -------------------------------------------------- extra-fd stdio (Windows)
// spawnExtra(command, args, opts, stdioCodes) -> {handle, pid} (synchronous,
// like Node's spawn). stdioCodes is a number per fd: 0=ignore 1=inherit
// 2=child-read(parent writes) 3=child-write(parent reads). Used for
// CDP-over-pipe so a browser child gets numbered fds 3/4.

#[cfg(any(windows, unix))]
fn op_spawn_extra(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    use oam_core::child_extra::StdioFd;
    let Some(command) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "spawnExtra: command required");
        return;
    };
    // A spawn entry point in its own right (extra-fd stdio), not a helper on
    // an already-permitted child -- so it carries the same gate.
    if !check_child_perm(scope, &command) {
        return;
    }
    let mut child_args: Vec<String> = Vec::new();
    if let Ok(arr) = v8::Local::<v8::Array>::try_from(args.get(1)) {
        for i in 0..arr.length() {
            if let Some(v) = arr.get_index(scope, i)
                && let Some(s) = v.to_string(scope)
            {
                child_args.push(s.to_rust_string_lossy(scope));
            }
        }
    }
    let opts = {
        let v = args.get(2);
        if v.is_null_or_undefined() {
            None
        } else {
            v8::Local::<v8::Object>::try_from(v).ok()
        }
    };
    let cwd = opts.and_then(|o| {
        let key = v8::String::new(scope, "cwd")?;
        let val = o.get(scope, key.into())?;
        if val.is_null_or_undefined() {
            return None;
        }
        val.to_string(scope).map(|s| s.to_rust_string_lossy(scope))
    });
    let clear_env = opts
        .and_then(|o| {
            let key = v8::String::new(scope, "clearEnv")?;
            o.get(scope, key.into())
        })
        .is_some_and(|v| v.is_true());
    let env_pairs: Option<Vec<(String, String)>> = opts.and_then(|o| {
        let key = v8::String::new(scope, "env")?;
        let val = o.get(scope, key.into())?;
        if val.is_null_or_undefined() {
            return None;
        }
        let env_obj = v8::Local::<v8::Object>::try_from(val).ok()?;
        let names = env_obj.get_own_property_names(scope, Default::default())?;
        let mut pairs = Vec::new();
        for i in 0..names.length() {
            if let Some(name) = names.get_index(scope, i)
                && let Some(val) = env_obj.get(scope, name)
                && let (Some(k), Some(v)) = (
                    name.to_string(scope).map(|s| s.to_rust_string_lossy(scope)),
                    val.to_string(scope).map(|s| s.to_rust_string_lossy(scope)),
                )
            {
                pairs.push((k, v));
            }
        }
        Some(pairs)
    });

    // stdio codes (arg 3): one number per fd. 0-3 are dispositions; anything
    // at or above DESCRIPTOR_CODE_BASE is `base + <descriptor number>`, the
    // `stdio: ['ignore','pipe','pipe', logFd]` form. The offset encoding keeps
    // the two meanings from overlapping -- a bare number would collide with the
    // disposition codes.
    const DESCRIPTOR_CODE_BASE: u32 = 1000;
    let files = core_runtime!(scope).sync_files();
    // Dups stay alive until spawn_extra has handed copies to the child; drop
    // closes ours and leaves the caller's descriptor untouched.
    let mut held: Vec<std::fs::File> = Vec::new();
    let mut stdio: Vec<StdioFd> = Vec::new();
    if let Ok(arr) = v8::Local::<v8::Array>::try_from(args.get(3)) {
        for i in 0..arr.length() {
            let code = arr
                .get_index(scope, i)
                .map(|v| v.uint32_value(scope).unwrap_or(0))
                .unwrap_or(0);
            if code >= DESCRIPTOR_CODE_BASE {
                let target = u64::from(code - DESCRIPTOR_CODE_BASE);
                oam_core::adopt_inherited_fd(&files, target);
                let dup = files
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .files
                    .get(&target)
                    .and_then(|f| f.try_clone().ok());
                match dup {
                    Some(file) => {
                        #[cfg(windows)]
                        let raw = {
                            use std::os::windows::io::AsRawHandle;
                            file.as_raw_handle() as isize
                        };
                        #[cfg(unix)]
                        let raw = {
                            use std::os::fd::AsRawFd;
                            file.as_raw_fd()
                        };
                        held.push(file);
                        stdio.push(StdioFd::Descriptor(raw));
                    }
                    // Unresolvable descriptor: same inherit fallback the 0/1/2
                    // path takes, rather than silently giving the child NUL.
                    None => stdio.push(StdioFd::Inherit),
                }
                continue;
            }
            stdio.push(match code {
                1 => StdioFd::Inherit,
                2 => StdioFd::ChildRead,
                3 => StdioFd::ChildWrite,
                _ => StdioFd::Ignore,
            });
        }
    }
    if stdio.is_empty() {
        throw_type_error(scope, "spawnExtra: stdio codes required");
        return;
    }

    let reg = core_runtime!(scope).raw_children();
    let ids = core_runtime!(scope).body_ids();
    match oam_core::child_extra::spawn_extra(
        &command,
        &child_args,
        cwd.as_deref(),
        env_pairs.as_deref(),
        clear_env,
        &stdio,
    ) {
        Ok(child) => {
            let handle = ids.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let pid = child.pid;
            reg.lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(handle, child);
            let json = serde_json::json!({ "handle": handle, "pid": pid });
            if let Some(s) = v8::String::new(scope, &json.to_string()) {
                rv.set(s.into());
            }
        }
        Err(msg) => throw_type_error(scope, &msg),
    }
}

#[cfg(any(windows, unix))]
fn op_spawn_extra_read(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(handle) = arg_child_handle(scope, &args, 0) else {
        throw_type_error(scope, "spawnExtraRead: valid child handle required");
        return;
    };
    let fd = args.get(1).uint32_value(scope).unwrap_or(0);
    let reg = core_runtime!(scope).raw_children();
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::child_extra::raw_read(reg, handle, fd),
    );
}

#[cfg(any(windows, unix))]
fn op_spawn_extra_write(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(handle) = arg_child_handle(scope, &args, 0) else {
        throw_type_error(scope, "spawnExtraWrite: valid child handle required");
        return;
    };
    let fd = args.get(1).uint32_value(scope).unwrap_or(0);
    let Some(data) = arg_bytes(scope, &args, 2) else {
        throw_type_error(scope, "spawnExtraWrite: data required");
        return;
    };
    let reg = core_runtime!(scope).raw_children();
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::child_extra::raw_write(reg, handle, fd, data),
    );
}

#[cfg(any(windows, unix))]
fn op_spawn_extra_close_fd(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(handle) = arg_child_handle(scope, &args, 0) else {
        throw_type_error(scope, "spawnExtraCloseFd: valid child handle required");
        return;
    };
    let fd = args.get(1).uint32_value(scope).unwrap_or(0);
    let reg = core_runtime!(scope).raw_children();
    oam_core::child_extra::raw_close_fd(&reg, handle, fd);
}

#[cfg(any(windows, unix))]
fn op_spawn_extra_wait(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(handle) = arg_child_handle(scope, &args, 0) else {
        throw_type_error(scope, "spawnExtraWait: valid child handle required");
        return;
    };
    let reg = core_runtime!(scope).raw_children();
    crate::ops::spawn_op(scope, &mut rv, oam_core::child_extra::raw_wait(reg, handle));
}

#[cfg(any(windows, unix))]
fn op_spawn_extra_kill(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(handle) = arg_child_handle(scope, &args, 0) else {
        throw_type_error(scope, "spawnExtraKill: valid child handle required");
        return;
    };
    let signal = arg_kill_signal(scope, &args, 1);
    let reg = core_runtime!(scope).raw_children();
    oam_core::child_extra::raw_kill(&reg, handle, signal);
}

// Non-Windows: extra-fd stdio is not implemented yet (Unix needs pre_exec
// dup2). Stubs keep the op table symbol-complete across platforms.
#[cfg(not(any(windows, unix)))]
fn op_spawn_extra(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    throw_type_error(
        scope,
        "spawnExtra: extra-fd stdio is only implemented on Windows",
    );
}
#[cfg(not(any(windows, unix)))]
fn op_spawn_extra_read(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    throw_type_error(scope, "spawnExtraRead: Windows only");
}
#[cfg(not(any(windows, unix)))]
fn op_spawn_extra_write(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    throw_type_error(scope, "spawnExtraWrite: Windows only");
}
#[cfg(not(any(windows, unix)))]
fn op_spawn_extra_close_fd(
    _scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
}
#[cfg(not(any(windows, unix)))]
fn op_spawn_extra_wait(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    throw_type_error(scope, "spawnExtraWait: Windows only");
}
#[cfg(not(any(windows, unix)))]
fn op_spawn_extra_kill(
    _scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
}

// ================================================================ cluster

fn op_cluster_fork(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(script_path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "clusterFork: script path required");
        return;
    };
    // Fork re-execs this runtime with `script_path` as argv, so the child is
    // gated on the script it will run -- the same escape hatch spawn would be.
    if !check_child_perm(scope, &script_path) {
        return;
    }
    let Some(worker_id) = arg_string(scope, &args, 1) else {
        throw_type_error(scope, "clusterFork: worker id required");
        return;
    };
    let env_pairs: Vec<(String, String)> = {
        let val = args.get(2);
        if val.is_null_or_undefined() {
            Vec::new()
        } else if let Ok(obj) = v8::Local::<v8::Object>::try_from(val) {
            let mut pairs = Vec::new();
            if let Some(names) = obj.get_own_property_names(scope, Default::default()) {
                for i in 0..names.length() {
                    if let Some(name) = names.get_index(scope, i)
                        && let Some(val) = obj.get(scope, name)
                        && let (Some(k), Some(v)) = (
                            name.to_string(scope).map(|s| s.to_rust_string_lossy(scope)),
                            val.to_string(scope).map(|s| s.to_rust_string_lossy(scope)),
                        )
                    {
                        pairs.push((k, v));
                    }
                }
            }
            pairs
        } else {
            Vec::new()
        }
    };
    let rt = core_runtime!(scope);
    let children = rt.children();
    let ids = rt.body_ids();
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::cluster::cluster_fork(children, ids, script_path, worker_id, env_pairs),
    );
}

fn op_cluster_worker_wait(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(handle) = arg_child_handle(scope, &args, 0) else {
        throw_type_error(scope, "clusterWorkerWait: valid child handle required");
        return;
    };
    let children = core_runtime!(scope).children();
    crate::ops::spawn_op(
        scope,
        &mut rv,
        oam_core::cluster::cluster_worker_wait(children, handle),
    );
}

fn op_cluster_worker_kill(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(handle) = arg_child_handle(scope, &args, 0) else {
        throw_type_error(scope, "clusterWorkerKill: valid child handle required");
        return;
    };
    let signal = arg_kill_signal(scope, &args, 1);
    let children = core_runtime!(scope).children();
    oam_core::cluster::cluster_worker_kill(&children, handle, signal);
}

fn op_cluster_is_worker(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let val = v8::Boolean::new(scope, oam_core::cluster::cluster_is_worker());
    rv.set(val.into());
}

// ================================================================ dns

fn validate_hostname(hostname: &str) -> Result<(), &'static str> {
    if hostname.is_empty() || hostname.len() > 253 {
        return Err("hostname must be between 1 and 253 characters");
    }
    if hostname.contains('\0') {
        return Err("hostname must not contain null bytes");
    }
    Ok(())
}

fn op_dns_lookup(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let hostname: String = args.get(0).to_rust_string_lossy(scope);
    if let Err(msg) = validate_hostname(&hostname) {
        throw_type_error(scope, msg);
        return;
    }

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

fn op_dns_resolve(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let hostname: String = args.get(0).to_rust_string_lossy(scope);
    if let Err(msg) = validate_hostname(&hostname) {
        throw_type_error(scope, msg);
        return;
    }
    let rrtype: String = args.get(1).to_rust_string_lossy(scope);
    crate::ops::spawn_op(scope, &mut rv, oam_core::dns::dns_resolve(hostname, rrtype));
}

fn op_dns_reverse(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let ip: String = args.get(0).to_rust_string_lossy(scope);
    crate::ops::spawn_op(scope, &mut rv, oam_core::dns::dns_reverse(ip));
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
    #[allow(clippy::upper_case_acronyms)]
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
    format!(
        "{}.{}.{}",
        info.dwMajorVersion, info.dwMinorVersion, info.dwBuildNumber
    )
}

#[cfg(not(windows))]
fn os_release() -> String {
    use std::process::Command;
    Command::new("uname")
        .arg("-r")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

#[cfg(windows)]
#[repr(C)]
#[allow(non_snake_case)]
#[allow(clippy::upper_case_acronyms)]
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

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
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
        (
            "total_heap_size_executable",
            stats.total_heap_size_executable(),
        ),
        ("total_physical_size", stats.total_physical_size()),
        ("total_available_size", stats.total_available_size()),
        ("used_heap_size", stats.used_heap_size()),
        ("heap_size_limit", stats.heap_size_limit()),
        ("malloced_memory", stats.malloced_memory()),
        ("peak_malloced_memory", stats.peak_malloced_memory()),
        (
            "does_zap_garbage",
            if stats.does_zap_garbage() { 1 } else { 0 },
        ),
        (
            "number_of_native_contexts",
            stats.number_of_native_contexts(),
        ),
        (
            "number_of_detached_contexts",
            stats.number_of_detached_contexts(),
        ),
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
        K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb);
    }
    counters.WorkingSetSize
}

#[cfg(target_os = "linux")]
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
        fn RegOpenKeyExW(
            hKey: isize,
            lpSubKey: *const u16,
            ulOptions: u32,
            samDesired: u32,
            phkResult: *mut isize,
        ) -> i32;
        fn RegQueryValueExW(
            hKey: isize,
            lpValueName: *const u16,
            lpReserved: *mut u32,
            lpType: *mut u32,
            lpData: *mut u8,
            lpcbData: *mut u32,
        ) -> i32;
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
            hkey,
            value_name.as_ptr(),
            ptr::null_mut(),
            &mut reg_type,
            buf.as_mut_ptr(),
            &mut buf_len,
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
        fn RegOpenKeyExW(
            hKey: isize,
            lpSubKey: *const u16,
            ulOptions: u32,
            samDesired: u32,
            phkResult: *mut isize,
        ) -> i32;
        fn RegQueryValueExW(
            hKey: isize,
            lpValueName: *const u16,
            lpReserved: *mut u32,
            lpType: *mut u32,
            lpData: *mut u8,
            lpcbData: *mut u32,
        ) -> i32;
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
            hkey,
            value_name.as_ptr(),
            ptr::null_mut(),
            &mut reg_type,
            &mut val as *mut u32 as *mut u8,
            &mut val_len,
        )
    };
    unsafe { RegCloseKey(hkey) };

    if result != 0 { 0 } else { val }
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
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
    use serde_json::{Map, Value, json};
    use std::net::UdpSocket;

    let mut result = Map::new();

    let lo_name = if cfg!(windows) {
        "Loopback Pseudo-Interface 1"
    } else {
        "lo"
    };
    result.insert(
        lo_name.to_string(),
        json!([
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
        ]),
    );

    if let Ok(socket) = UdpSocket::bind("0.0.0.0:0")
        && socket.connect("8.8.8.8:80").is_ok()
        && let Ok(addr) = socket.local_addr()
    {
        let ip = addr.ip().to_string();
        let iface_name = if cfg!(windows) { "Ethernet" } else { "eth0" };
        result.insert(
            iface_name.to_string(),
            json!([
                {
                    "address": ip,
                    "netmask": "255.255.255.0",
                    "family": "IPv4",
                    "mac": "00:00:00:00:00:00",
                    "internal": false,
                    "cidr": format!("{}/24", ip)
                }
            ]),
        );
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

#[cfg(target_os = "linux")]
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

// ================================ sysinfo-backed os/process natives (macOS +
// other non-linux unix). Windows uses FFI/registry above; linux reads /proc.
// sysinfo's API is platform-identical, so these compile and verify on Windows
// even though they are cfg-gated off there in the shipped build. A fresh
// System per call is fine -- these ops are infrequent and each refreshes only
// the slice it needs.

#[cfg(not(any(windows, target_os = "linux")))]
fn sysinfo_total_mem() -> u64 {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    sys.total_memory()
}

#[cfg(not(any(windows, target_os = "linux")))]
fn sysinfo_free_mem() -> u64 {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    sys.available_memory()
}

#[cfg(not(any(windows, target_os = "linux")))]
fn sysinfo_cpu_model() -> String {
    let mut sys = sysinfo::System::new();
    sys.refresh_cpu_all();
    sys.cpus()
        .first()
        .map(|c| c.brand().to_string())
        .unwrap_or_default()
}

#[cfg(not(any(windows, target_os = "linux")))]
fn sysinfo_cpu_speed_mhz() -> u32 {
    // Apple Silicon reports 0 here (no exposed CPU frequency); Node behaves
    // the same, so the e2e assertion is relaxed for darwin.
    let mut sys = sysinfo::System::new();
    sys.refresh_cpu_all();
    sys.cpus()
        .first()
        .map(|c| c.frequency() as u32)
        .unwrap_or(0)
}

#[cfg(not(any(windows, target_os = "linux")))]
fn sysinfo_process_rss() -> usize {
    use sysinfo::{ProcessesToUpdate, get_current_pid};
    let Ok(pid) = get_current_pid() else {
        return 0;
    };
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    sys.process(pid).map(|p| p.memory() as usize).unwrap_or(0)
}

#[cfg(not(any(windows, target_os = "linux")))]
fn sysinfo_parent_pid() -> u32 {
    use sysinfo::{ProcessesToUpdate, get_current_pid};
    let Ok(pid) = get_current_pid() else {
        return 0;
    };
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    sys.process(pid)
        .and_then(|p| p.parent())
        .map(|pp| pp.as_u32())
        .unwrap_or(0)
}

#[cfg(not(any(windows, target_os = "linux")))]
fn total_mem() -> u64 {
    sysinfo_total_mem()
}

#[cfg(not(any(windows, target_os = "linux")))]
fn free_mem() -> u64 {
    sysinfo_free_mem()
}

#[cfg(not(any(windows, target_os = "linux")))]
fn process_rss() -> usize {
    sysinfo_process_rss()
}

#[cfg(not(any(windows, target_os = "linux")))]
fn cpu_model() -> String {
    sysinfo_cpu_model()
}

#[cfg(not(any(windows, target_os = "linux")))]
fn cpu_speed_mhz() -> u32 {
    sysinfo_cpu_speed_mhz()
}

#[cfg(not(any(windows, target_os = "linux")))]
fn parent_pid() -> u32 {
    sysinfo_parent_pid()
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
    #[allow(clippy::upper_case_acronyms)]
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
        ru_utime: timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        ru_stime: timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
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

    // Only child processes of this runtime should be killable.
    let children = core_runtime!(scope).children();
    let is_own_child = children
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .any(|child| child.pid == pid);
    let is_self = pid == std::process::id();
    if !is_own_child && !is_self {
        throw_type_error(
            scope,
            &format!("process.kill: permission denied for pid {pid} (not a child process)"),
        );
        return;
    }

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

#[cfg(test)]
mod permission_audit {
    // ---------------------------------------------------- permission-gate audit
    //
    // A SOURCE-LEVEL gate on the permission model. Every op that reaches the
    // filesystem, spawns a process, or starts a child isolate must either perform
    // a permission check or be listed below with the reason it does not need one.
    //
    // Why this exists rather than trusting review: the checks were added by hand
    // and drifted badly -- 38 of 47 fs ops had none, and `child_process` had none
    // at all, so `--permission` could be escaped with a single `execSync`. Nothing
    // failed when an op was added without a check, which is precisely the shape of
    // bug that ships. This test fails the moment a new op appears without one.
    #[test]
    fn every_sensitive_op_checks_permissions_or_is_explicitly_exempt() {
        // fd-based ops are exempt BY CAPABILITY: the descriptor can only have come
        // from `open`, which is checked, so re-checking a path here would be
        // checking a path the caller no longer names. Node draws the line in the
        // same place. Anything added to this list needs a reason of that quality.
        const EXEMPT: &[(&str, &str)] = &[
            ("op_fs_read_chunk", "fd from a checked open()"),
            ("op_fs_write_chunk", "fd from a checked open()"),
            ("op_fs_close", "fd from a checked open()"),
            ("op_fs_read_sync", "fd from a checked open()"),
            ("op_fs_write_sync", "fd from a checked open()"),
            ("op_fs_close_sync", "fd from a checked open()"),
            ("op_fs_fstat", "fd from a checked open()"),
            ("op_fs_fstat_sync", "fd from a checked open()"),
            ("op_fs_fsync", "fd from a checked open()"),
            ("op_fs_fsync_sync", "fd from a checked open()"),
            ("op_fs_fdatasync", "fd from a checked open()"),
            ("op_fs_fdatasync_sync", "fd from a checked open()"),
            ("op_fs_ftruncate", "fd from a checked open()"),
            ("op_fs_ftruncate_sync", "fd from a checked open()"),
            ("op_fs_fchmod", "fd from a checked open()"),
            ("op_fs_fchmod_sync", "fd from a checked open()"),
            ("op_fs_fchown", "fd from a checked open()"),
            ("op_fs_fchown_sync", "fd from a checked open()"),
            ("op_fs_futimes", "fd from a checked open()"),
            ("op_fs_futimes_sync", "fd from a checked open()"),
            (
                "op_spawn_kill",
                "signals a child already permitted at spawn",
            ),
            (
                "op_spawn_read_stdout",
                "reads a pipe of an already-spawned child",
            ),
            (
                "op_spawn_read_stderr",
                "reads a pipe of an already-spawned child",
            ),
            ("op_spawn_write", "writes stdin of an already-spawned child"),
            (
                "op_spawn_close_stdin",
                "closes a pipe of an already-spawned child",
            ),
            ("op_spawn_wait", "awaits a child already permitted at spawn"),
            (
                "op_spawn_extra_read",
                "reads a pipe of an already-spawned child",
            ),
            (
                "op_spawn_extra_write",
                "writes a pipe of an already-spawned child",
            ),
            (
                "op_spawn_extra_close_fd",
                "closes a pipe of an already-spawned child",
            ),
            (
                "op_spawn_extra_wait",
                "awaits a child already permitted at spawn",
            ),
            (
                "op_spawn_extra_kill",
                "signals a child already permitted at spawn",
            ),
            (
                "op_cluster_worker_wait",
                "awaits a child already permitted at fork",
            ),
            (
                "op_cluster_worker_kill",
                "signals a child already permitted at fork",
            ),
            (
                "op_cluster_is_worker",
                "reads an env flag set by the forked parent",
            ),
        ];

        let source = include_str!("node_ops.rs");
        let mut offenders = Vec::new();

        // Split on top-level `fn ` boundaries; each chunk is one op body.
        let mut current: Option<&str> = None;
        let mut body = String::new();
        let check = |name: Option<&str>, body: &str, offenders: &mut Vec<String>| {
            let Some(name) = name else { return };
            let sensitive = name.starts_with("op_fs_")
                || name.starts_with("op_spawn_")
                || name.starts_with("op_cluster_")
                || name.starts_with("op_process_exec");
            if !sensitive || EXEMPT.iter().any(|(n, _)| *n == name) {
                return;
            }
            // Only ops that actually PERFORM the operation need a gate. The
            // `#[cfg(not(windows))]` twins are pure `throw_type_error("... only
            // implemented on Windows")` stubs -- they touch nothing, so demanding
            // a check of them would just be noise. Keyed on whether the body
            // reaches an operation surface at all.
            // The ONLY ops excused here are the `#[cfg(not(windows))]` twins:
            // pure `throw_type_error("... only implemented on Windows")`
            // stubs that touch nothing.
            //
            // Deliberately a denylist of that one shape rather than an
            // allowlist of operation surfaces (`std::fs`, `oam_core::`, ...).
            // The allowlist version silently skipped `op_fs_exists_sync`,
            // which reaches the filesystem through `std::path::Path::exists`,
            // and `op_fs_open_sync` -- both were checked at the time, so the
            // gate looked green while covering neither. An allowlist of API
            // surfaces can never be complete; the stub shape is a closed set.
            let is_unsupported_stub = body.contains("throw_type_error")
                && (body.contains("only implemented on") || body.contains("Windows only"));
            if is_unsupported_stub {
                return;
            }
            let checked = body.contains("check_read_perm")
            || body.contains("check_write_perm")
            || body.contains("check_child_perm")
            || body.contains("check_worker_perm")
            // Direct form: `get_permissions(scope).check_child(..)`.
            || body.contains(".check_read(")
            || body.contains(".check_write(")
            || body.contains(".check_child(")
            || body.contains(".check_worker(");
            if !checked {
                offenders.push(name.to_string());
            }
        };

        for line in source.lines() {
            if let Some(rest) = line.strip_prefix("fn ")
                && let Some(name) = rest.split('(').next()
            {
                check(current, &body, &mut offenders);
                current = Some(name);
                body.clear();
                continue;
            }
            body.push_str(line);
            body.push('\n');
        }
        check(current, &body, &mut offenders);

        assert!(
            offenders.is_empty(),
            "these ops touch the filesystem or spawn without a permission check:\n  {}\n\n\
         Add the appropriate check_*_perm call, or -- if the op is safe by \
         capability (it only handles an fd or child already gated at creation) \
         -- add it to EXEMPT in this test with the reason.",
            offenders.join("\n  ")
        );
    }
}
