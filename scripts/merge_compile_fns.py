#!/usr/bin/env python3
"""Merge compile function bodies and unit test into main.rs."""
import pathlib, sys

p = pathlib.Path("crates/oam_cli/src/main.rs")
src = p.read_text(encoding="utf-8")

# The compile functions to insert before #[cfg(test)]
compile_fns = r'''
// -- oam compile: embed a pre-bundled JS file into a standalone binary --

/// 8-byte magic trailer written after the JS payload + length.
/// Format: [JS bytes][u64 LE length][b"OAMEXEC\0"]
const COMPILE_MAGIC: &[u8; 8] = b"OAMEXEC\0";

/// Read the last 16 bytes of the current executable to check for an
/// embedded JS payload. Returns `Some(source)` if the magic marker and
/// length are valid, `None` otherwise (normal CLI binary).
fn extract_embedded_js() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let mut f = std::fs::File::open(&exe).ok()?;
    let file_len = f.metadata().ok()?.len();
    // Trailer is 16 bytes: 8 (length) + 8 (magic).
    if file_len < 16 {
        return None;
    }
    let mut trailer = [0u8; 16];
    f.seek(SeekFrom::End(-16)).ok()?;
    f.read_exact(&mut trailer).ok()?;
    // Check magic marker (last 8 bytes of trailer).
    if &trailer[8..16] != COMPILE_MAGIC {
        return None;
    }
    let js_len = u64::from_le_bytes(trailer[0..8].try_into().unwrap());
    // Sanity: JS payload + 16-byte trailer must fit in the file.
    if js_len > file_len - 16 {
        return None;
    }
    let offset = file_len - 16 - js_len;
    f.seek(SeekFrom::Start(offset)).ok()?;
    let mut buf = vec![0u8; js_len as usize];
    f.read_exact(&mut buf).ok()?;
    String::from_utf8(buf).ok()
}

/// Execute embedded JS source as a CJS script (the typical output of
/// esbuild/rollup --format=cjs). Supports `--inspect` / `--inspect-brk`
/// flags for debugging the compiled binary.
fn run_embedded(source: &str, args: Vec<String>) -> ExitCode {
    // Parse --inspect / --inspect-brk from raw args (we bypass clap for
    // embedded binaries so the user's positional args pass through).
    let mut inspect: Option<(std::net::SocketAddr, bool)> = None;
    let mut script_args: Vec<String> = Vec::new();
    let mut iter = args.iter().skip(1); // skip argv[0]
    #[allow(clippy::while_let_on_iterator)] // need iter.cloned() inside the loop body
    while let Some(arg) = iter.next() {
        if arg == "--inspect-brk" || arg.starts_with("--inspect-brk=") {
            let value = if let Some(v) = arg.strip_prefix("--inspect-brk=") {
                v.to_string()
            } else {
                "127.0.0.1:9229".to_string()
            };
            match resolve_inspect(None, Some(&value)) {
                Ok(v) => inspect = v,
                Err(e) => {
                    eprintln!("{e}");
                    return ExitCode::FAILURE;
                }
            }
        } else if arg == "--inspect" || arg.starts_with("--inspect=") {
            let value = if let Some(v) = arg.strip_prefix("--inspect=") {
                v.to_string()
            } else {
                "127.0.0.1:9229".to_string()
            };
            match resolve_inspect(Some(&value), None) {
                Ok(v) => inspect = v,
                Err(e) => {
                    eprintln!("{e}");
                    return ExitCode::FAILURE;
                }
            }
        } else if arg == "--" {
            script_args.extend(iter.cloned());
            break;
        } else {
            script_args.push(arg.clone());
        }
    }

    let mut rt = oam_engine::JsRuntime::new();
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "oam-compiled".to_string());
    let mut argv = vec![exe];
    argv.extend(script_args);
    rt.set_process_argv(argv);

    if let Some((addr, brk)) = inspect {
        match rt.attach_inspector(addr, brk) {
            Ok(url) => {
                eprintln!("Debugger listening on {url}");
                eprintln!("For help, see: https://oam.sh/docs/inspector");
            }
            Err(e) => {
                eprintln!("could not start inspector: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    // Write the embedded source to a temp file so the CJS loader has a
    // real path for __filename / __dirname / require() resolution.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let tmp_dir = std::env::temp_dir().join(format!("oam-embed-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).expect("create temp dir for embedded source");
    let tmp_file = tmp_dir.join("__oam_embedded.js");
    std::fs::write(&tmp_file, source).expect("write embedded source to temp");

    let result = rt.execute_cjs(&tmp_file);
    let _ = std::fs::remove_dir_all(&tmp_dir);
    match result {
        Ok(()) => ExitCode::from(rt.process_exit_code().unwrap_or(0).clamp(0, 255) as u8),
        Err(diagnostics) => {
            for d in &diagnostics {
                render(d, false);
            }
            ExitCode::FAILURE
        }
    }
}

/// `oam compile <entry> --output <path>`: read the JS source, copy the
/// current oam binary, and append the JS payload with a magic trailer.
fn compile_command(entry: &Path, output: &Path) -> ExitCode {
    // 1. Read the entry JS file.
    let source = match std::fs::read(entry) {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("oam compile: could not read {}: {e}", entry.display());
            return ExitCode::FAILURE;
        }
    };

    // Validate it's UTF-8 (JS source must be).
    if std::str::from_utf8(&source).is_err() {
        eprintln!(
            "oam compile: {} is not valid UTF-8 (expected a JS source file)",
            entry.display()
        );
        return ExitCode::FAILURE;
    }

    // 2. Copy the current oam binary to the output path.
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("oam compile: could not locate own executable: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                eprintln!(
                    "oam compile: could not create output directory {}: {e}",
                    parent.display()
                );
                return ExitCode::FAILURE;
            }
        }
    }
    if let Err(e) = std::fs::copy(&exe, output) {
        eprintln!(
            "oam compile: could not copy binary to {}: {e}",
            output.display()
        );
        return ExitCode::FAILURE;
    }

    // 3. Append: [JS source bytes][u64 LE length][magic "OAMEXEC\0"]
    let mut out_file = match std::fs::OpenOptions::new().append(true).open(output) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("oam compile: could not open {} for append: {e}", output.display());
            return ExitCode::FAILURE;
        }
    };
    use std::io::Write;
    let js_len = source.len() as u64;
    if let Err(e) = out_file
        .write_all(&source)
        .and_then(|()| out_file.write_all(&js_len.to_le_bytes()))
        .and_then(|()| out_file.write_all(COMPILE_MAGIC))
    {
        eprintln!("oam compile: write failed: {e}");
        let _ = std::fs::remove_file(output);
        return ExitCode::FAILURE;
    }

    let out_abs = std::path::absolute(output)
        .unwrap_or_else(|_| output.to_path_buf());
    eprintln!(
        "oam compile: {} ({} bytes JS) -> {}",
        entry.display(),
        source.len(),
        out_abs.display()
    );
    ExitCode::SUCCESS
}

'''

# Insert before #[cfg(test)]
marker = '#[cfg(test)]\nmod tests {'
assert marker in src, f'could not find #[cfg(test)] mod tests marker'
src = src.replace(marker, compile_fns + marker, 1)

# Add compile_magic_is_8_bytes test before the closing brace of mod tests
test_fn = '''
    #[test]
    fn compile_magic_is_8_bytes() {
        assert_eq!(super::COMPILE_MAGIC.len(), 8);
        assert_eq!(super::COMPILE_MAGIC, b"OAMEXEC\\0");
    }
'''

# Find the last closing brace (end of mod tests)
last_brace = src.rstrip().rfind('}')
src = src[:last_brace] + test_fn + src[last_brace:]

p.write_text(src, encoding='utf-8')
print('OK -- added compile functions + unit test')
