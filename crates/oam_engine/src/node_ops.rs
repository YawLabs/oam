//! Native bindings for the node: compat layer: `__oam.node`.
//!
//! The JS half (js/node_compat.js factories) is pure and lives in the
//! snapshot; everything here is installed after restore. Sync fs natives
//! call std::fs directly on the isolate thread (that is what Sync means);
//! async fs natives ride the oam_core op channel like fetch does. All fs
//! failures throw/reject Errors carrying Node's `.code` (ENOENT, ...) —
//! the ecosystem branches on codes, not messages.

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
    let data: [(&str, v8::Local<v8::Value>); 5] = [
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
        // AsyncLocalStorage substrate: V8's continuation-preserved embedder
        // data, propagated across promise continuations by V8 itself.
        ("getContinuationData", op_get_continuation_data),
        ("setContinuationData", op_set_continuation_data),
        // fs sync
        ("fsReadFileSync", op_fs_read_file_sync),
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
    );

    let node_key = v8::String::new(scope, "node").unwrap();
    internal.set(scope, node_key.into(), node.into());
}

// ----------------------------------------------------------------- helpers

fn throw_type_error(scope: &mut v8::PinScope<'_, '_>, message: &str) {
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

fn arg_string(
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
fn arg_bytes(
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

fn bytes_to_uint8array<'s>(
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

fn op_fs_write_file_sync(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(path) = arg_string(scope, &args, 0) else {
        throw_type_error(scope, "writeFileSync requires a path");
        return;
    };
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
