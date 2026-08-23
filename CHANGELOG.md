# Changelog

All notable changes to oam are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Entries are derived from the commit history between release tags. Pure internal
churn — formatting passes, lockfile syncs, benchmark receipt refreshes — is
omitted. oam is pre-1.0: breaking changes can land in a minor release.

Version numbering is not contiguous: **0.9.3 and 0.9.5 were never tagged**, so a
gap between headings below is expected rather than a missing entry. Not every tag
is a published GitHub Release either — 0.9.2 through 0.10.1 were tagged without
one, so `install.sh`, which resolves the latest Release, never handed them out.

## [Unreleased]

### Fixed

- **`oam.exe` no longer needs the VC++ redistributable.** The Windows builds link
  the static CRT, so a fresh machine can run a downloaded binary without first
  installing Microsoft's runtime. (#66)
- The release pipeline reclaims builder disk before a build, and two IAP tunnel
  warnings that fired on every remote build are fixed. `scripts/` is now covered
  by its own gate. (#67)

### Changed

- `--allow-worker` is documented as no longer implying `--allow-child-process` —
  it stopped implying it in 0.9.1, when child isolates began inheriting the
  parent's grants. (#65)
- The `unsafe` coverage gate is enforced through clippy, and the
  `documented_count` floor it replaced is retired. (#63)

## [0.11.0] - 2026-08-22

A **hardening** release: three memory-safety fixes reachable from ordinary JS or an
ordinary spawn, one Windows handle-scoping fix that matters to anyone launching child
processes concurrently, and the `unsafe` audit gate going from advisory to gating.

### Fixed

- **A Windows child inherited every inheritable handle in the process.** `spawn_extra`
  called `CreateProcessW` with `bInheritHandles=TRUE` and no handle list, so a
  concurrent spawn could leak our pipe ends into an unrelated child — the peer then
  never saw EOF (a hang), and the handle was exposed to a process that had no business
  holding it. The extra-fd spawn now passes an explicit `STARTUPINFOEXW` +
  `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`. If you launch several sidecars at once on
  Windows, this is the fix you want. (#55)
- **`fsReadSync` and `zlibHandleWriteSync` trusted a JS-supplied offset and length.**
  Both are reachable from ordinary user JS through `__oam.node` — no native addon
  required — and an unvalidated value produced an out-of-bounds write or an
  abort-the-process allocation. Both now bounds-check with `checked_add` and clamp the
  allocation. (#52)
- **A closed inherited descriptor could be re-adopted after the kernel reused its
  number.** The adoption path keyed on the raw fd, so once the OS recycled it, a second
  `closeSync(n)` closed a descriptor the runtime had since opened for itself. A closed
  inherited fd now stays closed and reports `EBADF`. (#53)
- **N-API deferred and addon lifetimes.** `napi_resolve_deferred` /
  `napi_reject_deferred` freed the caller-owned deferred on the invalid-argument path,
  so a caller that checked the status and retried hit a use-after-free or double-free;
  and `load_addon` unmapped the library when `register()` failed while the env still
  held code pointers into it. Both are reachable only with
  `OAM_ENABLE_NATIVE_ADDONS=1` — native addons remain off by default. (#54)

### Changed

- **The `unsafe` audit gate now gates.** CI gate 5 was an advisory `grep -c unsafe`;
  it is now `cargo run -p xtask -- unsafe-budget`, a bidirectional ratchet keyed on
  `conformance/unsafe-budget.json` that fails both on new undocumented `unsafe` and on
  a stale budget that no longer matches. `forbid(unsafe_code)` is set on the three
  crates that have none, `undocumented_unsafe_blocks` is denied outside `oam_engine`,
  and `oam_engine`'s own blocks are documented — the coverage floor moved 10 → 575.
  `--regen` prints a diff of what it is about to re-bless rather than overwriting
  silently. No runtime behavior changes. (#56, #58–#62)

## [0.10.2] - 2026-08-16

### Changed

- **`fs.glob` stops honoring a caller's `nocase`, because Node does not.** Node v22
  validates only `cwd` / `exclude` / `withFileTypes` and silently ignores the rest, so
  case folding is a property of the platform, not of the options object. Verified
  against v22.22.2: on win32 `FOO.*` matches `foo.js` even with `nocase: false`, and on
  Linux `nocase: true` still returns only the exact-case match. oam now derives folding
  from the platform (win32 or darwin) and ignores the option. **If you passed
  `nocase: true` on a case-sensitive filesystem and relied on it, that stopped working
  here.** Literal (non-globstar) segments are now emitted pattern-cased rather than
  listing-cased, so `SUB/*.js` yields `SUB\inner.js` on win32.
- The remaining oam-only glob extensions are now written down as **divergence 21** in
  `docs/node-divergences.md`: oam implements `nodir`, `include`, `follow` and the
  oam-only `maxResults`, all of which Node silently ignores — so passing one produces
  glob-package behavior on oam and no behavior on Node. Passing none is identical on
  both. Whether to drop them for strict parity is left open.

## [0.10.1] - 2026-08-16

An **extra-fd `child_process`** release — the descriptor path a CDP browser transport
runs on. Everything here was found by writing the coverage that was missing rather than
by anything failing in the field.

### Fixed

- **A live `kill()` on an extra-fd child was a silent no-op on Unix.** `raw_kill` only
  signaled while the registry still held the `Child` handle, but `raw_wait` takes that
  handle at its first poll — and the wait op starts the moment the child spawns — so
  every real kill arrived after the handle was gone and the child ran on untouched
  (conformance case 68's sigterm leg timed out on macOS; **a CDP browser child could
  not be terminated at all**). Liveness now comes from a `reaped` flag flipped under the
  registry lock when the blocking wait returns: a kill is delivered any time before the
  reap (a zombie discards it harmlessly) and never after (a reaped pid can be recycled
  by the kernel). Windows was never affected — `child_win.rs` copies the handle. (#49)
- **A failed extra-fd spawn emitted `error` and nothing else**, so a caller whose
  completion path is `close` — a CDP driver probing for a browser binary that is not
  installed — stalled forever. The extra-fd path now follows `error` with `close`
  carrying the libuv errno, the same contract the plain path keeps, and both raw
  native backends now include `errno` in the failure body so that argument can be
  node's (Windows additionally routes `GetLastError` through the shared mapping, so
  `ERROR_PATH_NOT_FOUND` reports `ENOENT` instead of `UNKNOWN`). (#51)
- **`kill()` with a numeric signal was handed to the native layer as a stringified
  number** no signal table recognizes, so `kill(9)` silently delivered SIGTERM and an
  invalid number like `kill(987654)` threw nothing. Numbers are now validated against
  `os.constants.signals` and converted to their canonical name — node's rule: a number
  is valid iff it appears in the platform's signal mapping, anything else is
  `ERR_UNKNOWN_SIGNAL`. Note oam carries the POSIX signal table on every platform
  (divergence 14), so a Windows-specific number may now throw where it previously
  "worked" as SIGTERM. (#51)
- `fs.glob`'s `nodir` filter is now applied before segment matching rather than after.
  No change in output; it stops the walk doing work it was about to discard.

### Changed

- The conformance `surface-gaps.json` linux section ratchets down six glob names. (#50)

## [0.10.0] - 2026-08-15

### Fixed

- **A child that trapped your signal and exited cleanly was reported as killed by it.**
  The extra-fd `raw_wait` synthesized its exit report instead of reading the real
  `ExitStatus`, so a child that caught SIGUSR1 and exited 0 came back as
  `{code: null, signal: "SIGUSR1"}` rather than `{code: 0, signal: null}`.
- **`ChildProcess.kill()` now throws `ERR_UNKNOWN_SIGNAL`** for an unrecognized signal
  *name* instead of silently falling back to SIGTERM.
- `fs.glob`'s globstar `exclude` and empty-match semantics were rewritten against
  node's own `lib/internal/fs/glob.js`: a function-valued `exclude` is called with the
  entry's leaf name during globstar iteration, as node does.
- The Windows release build could fail with `LNK1104` when a previous `oam.exe` was
  still mapped; `deps/oam.exe` is now parked alongside `release/oam.exe` before linking.

### Changed

- The `.cts` entry was moved into **[0.6.1]**, where it actually shipped, and the stale
  `OAM-MOD0003` explanation that told users to "write ESM TypeScript instead" was
  corrected — `.cts` runs through CJS interop with TS strip, and `.tsx`/`.jsx` run
  through the JSX automatic runtime.

## [0.9.8] - 2026-08-15

### Fixed

- `child_kill` recorded a kill against a child that had already exited but not yet been
  reaped. `kill(2)` on a zombie returns 0, so the result was reported as
  `{code: null, signal: "SIGTERM"}` instead of the child's real `{code: 0, signal: null}`.

## [0.9.7] - 2026-08-15

### Fixed

- A stolen or already-consumed extra-fd read end broke out of the read loop instead of
  taking the EOF path, so `'end'` never fired and `'close'` arrived without it.

## [0.9.6] - 2026-08-15

### Fixed

- The extra-fd pump let `'close'` beat `'end'` on a fast exit. A child that wrote fd4,
  closed it and exited inside a single 50 ms peek cycle lost its `'end'` entirely
  (conformance case 67: "fd4 saw EOF false").

## [0.9.4] - 2026-08-15

### Added

- `SIGCONT` and `SIGTSTP` are mappable in `process.on(...)` on Unix, built from the
  platform's `libc` numbers. `SIGTSTP` joins the "default terminates" restore-and-re-raise
  set; `SIGCONT` deliberately does not.

## [0.9.2] - 2026-08-15

Two resource caps, because the failure they replace was an ungraceful abort with no
exit path.

### Added

- **`fs.glob`, `fs.globSync`, `fsPromises.glob` and `path.matchesGlob`.** Before this
  release these were missing export *names*, which is link-time death for anything that
  imports them (see [0.9.1]).
- **A default V8 heap cap of 4 GiB**, resolved as: unset or empty → 4 GiB;
  `OAM_MAX_HEAP_MB=0` → no cap; a non-numeric value → no cap (typo-safe); `n > 0` → n MiB.
  There was previously no cap, but V8 aborted near 1.4 GiB with "Ineffective
  mark-compacts near heap limit" and no way to exit cleanly — so this *raises* the
  ceiling and converts the abort into a deterministic
  `error[OAM-RT-OOM]` on **stderr** with exit code **134**, uniform on every platform.
  stdout is deliberately left clean so an MCP sidecar's protocol channel is not
  corrupted on the way out, and the banner distinguishes a cap set by `OAM_MAX_HEAP_MB`
  from the 4 GiB default.
- **A 1,000,000-match cap on glob results**, applied after dedup, overridable with
  `maxResults` (`Infinity` disables). Exceeding it throws `ERR_OUT_OF_RANGE`, and a
  non-number `maxResults` throws `ERR_INVALID_ARG_TYPE` — previously passing `0`
  silently meant a million.

### Fixed

- Concurrent reads on a child's stdout/stderr now take a `busy` flag under one lock. A
  second concurrent read used to see a false EOF, which stalled the pump's `'close'`.
- Native child ops throw a `TypeError` for a non-numeric or NaN handle argument instead
  of coercing it to handle `0` — which was a real child.
- `oam.fork()`'s re-exec regained its `check_child_perm` call. Both halves landed inside
  this release, so no tagged build shipped the gap.

## [0.9.1] - 2026-08-11

A builtin **export-surface** release. `import { statfsSync } from "node:fs"` used to kill
a program on oam before it ran a line — a builtin's ESM named exports are its module
object's own enumerable keys, so a name oam had not implemented failed at *link* time,
taking down bundled CLIs that never called the function. Measuring the whole surface
found 384 such names across 41 builtins; the ones below are closed and the rest are now
tracked by a gate instead of waiting to be discovered by a crash.

### Added

- `fs.statfs`, `fs.statfsSync` and `fsPromises.statfs`, backed by a real syscall
  (`GetDiskFreeSpaceW` on Windows, `statfs(2)` on Linux/macOS, `statvfs` elsewhere), with
  the `StatFs` shape, the `bigint: true` option, and Node's `ENOENT`/`statfs` error shape.
  Values cross the native boundary as decimal strings so bigint mode is exact rather than
  rounded through a double.
- `node:console` now exports its full surface. It was built with
  `Object.create(globalThis.console)`, which left every method on the prototype, so the
  module's only named export was `Console` and `import { log } from "node:console"` was a
  hard failure. The module is now the global console itself, as it is in Node, and gained
  `dirxml`, `profile`, `profileEnd`, `timeStamp`, `createTask` and `context`.
- `fs.Dir` is a real exported class shared by `opendir` and `opendirSync`; each form
  previously returned an ad-hoc object with only half the method set. Closed handles now
  throw `ERR_DIR_CLOSED`, and iterating (or `break`ing out of) a `for await` closes the
  handle, both matching Node.
- `fs.fstat` (the async callback form), `fs.unwatchFile`, `fs._toUnixTimestamp`, and the
  top-level `F_OK`/`R_OK`/`W_OK`/`X_OK` re-exports.
- **fd-based ops**: `fsync`, `fdatasync`, `ftruncate`, `fchmod`, `fchown`, `futimes`, in
  both callback and sync form. On Windows `fchown` is a success-reporting no-op, matching
  libuv, so portable code need not branch. (#43)
- **Path-based ownership and time ops**: `chown`, `lchown`, `utimes`, `lutimes`, `lchmod`
  across `node:fs` and `node:fs/promises`. Node's asymmetries are reproduced rather than
  inferred — `chown` on Windows succeeds for a nonexistent path, the error syscall is the
  singular `utime`/`lutime`, and `lchmod` is bound to `undefined` off macOS in `node:fs`
  while `fs/promises` always exposes it and rejects with `ERR_METHOD_NOT_IMPLEMENTED`. (#44)
- **`fs.readv` / `fs.writev`**, and `fs.openAsBlob`. (#47, #48)

### Security

- **`--permission` is enforced across the whole `fs` surface.** Only 9 of 47 fs ops
  checked it: under `--permission` with no grants, `fs.unlinkSync` deleted the file,
  `fs.promises.rename`/`mkdir`/`chmod` succeeded, and `fs.promises.stat`/`readdir`
  enumerated the filesystem. Every path-based op now checks, with read/write classified
  to match Node's own model (`copyFile` reads the source and writes the destination;
  `rename` writes both; `link` reads the existing name and writes the new one).
  fd-based ops (`read`/`write`/`close`/`fstat`) are intentionally unchecked — the
  descriptor can only have come from `open`, which is checked, so the capability is
  already gated. **One deliberate divergence:** oam also gates `fs.realpath`, which Node
  permits without a read grant; a sandbox should not leak path existence.
- **`child_process` and `worker_threads` no longer escape `--permission`.** Neither
  consulted the permission model: `execSync` ran anything, and a `Worker` (or an
  `oam.fork()` isolate) was constructed with all-granted permissions regardless of the
  parent's flags, so either was a one-line bypass of every fs and net restriction. Spawn
  now checks `child` (including the extra-fd spawn path), starting an isolate checks the
  new `worker` permission, and a child isolate INHERITS the parent's set. Because of that
  inheritance, `--allow-worker` no longer implies `--allow-child-process`; node keeps them
  separate too.
- **`--permission` was silently ignored by `oam repl` and `oam test`.** Both built an
  all-granted runtime, because permissions are fixed at *construction* and
  `flags.install()` cannot retrofit them — so `oam --permission test suspect.test.js`
  ran with full disk, network and spawn access. A source-scanning test now pins one
  allowed all-granted call site and four permissioned entry points. (#42)
- A source-level test now fails the build if any op that touches the filesystem, spawns a
  process, or starts an isolate ships without a permission check or an explicit,
  reasoned exemption. It caught a second spawn entry point while being written.

### Fixed

- Async `fs` rejections carry node's full system-error shape. `OpOutcome::NodeFailed`
  held only `{code, message}`, so every promise-form failure had `syscall`, `errno` and
  `path` undefined while its sync twin set all four — packages that branch on
  `err.syscall === "open"` or read `err.path` (graceful-fs, chokidar, rimraf) saw
  nothing there.
- **An fd now crosses the sync/async boundary.** oam kept two open-file registries — a
  tokio-backed one for async ops and a std-backed one for sync ops — so a descriptor from
  async `open` threw `EBADF` in `readSync`/`fstatSync`/`closeSync`, and the reverse failed
  too. Node has one descriptor space and real code mixes the families freely
  (`fsPromises.open()` then `readSync(fh.fd, ...)`). Unified onto one registry. (#46)
- **Positional `read`/`write` moved the fd cursor.** Node's are `pread(2)`/`pwrite(2)` and
  leave it alone, so `readSync(fd,b,0,3,10)` followed by `readSync(fd,b,0,3,null)` returned
  `"KLM"`/`"NOP"` where node returns `"KLM"`/`"ABC"` — silent wrong bytes, not an error.
  Fixed on the write side too, and fs errors now enumerate as `["errno","code","syscall"]`
  in node's order, which `Object.keys` makes observable. (#45)
- **A failed async read or write destroyed the descriptor.** The error arm never
  reinstated the `File`, so an fd that reported one transient error was dead thereafter —
  code that handled the error and retried on the same descriptor worked on Node and could
  not work here. The async `fs.read` also ignored its `position` argument outright,
  reading a different region silently, and capped at 8 MiB so a 10 MiB `readv` short-read. (#48)
- **Blob internals were own-enumerable**, so `JSON.stringify(blob)` dumped the entire
  payload as `{"_bytes":{"0":104,...}}` — any log line or API response carrying a Blob
  serialized the whole file. Fixed for every Blob, not just `openAsBlob`'s. (#47)
- `'close'` could beat stdout's `'end'` under load. The child's exit now waits on
  `readableFinished()`, but only when something is actually consuming the stream (flowing,
  piped, or a `'readable'` listener), so an ignore-the-output spawn still cannot hang.
- `fs.truncate`/`truncateSync` reported `syscall: "truncate"`; node reports the syscall
  that actually failed, so a missing path is `open` and a failed resize is `ftruncate`.
- `watcher.close()` on a `fs.watchFile` handle stopped *every* watcher on that path when
  the listener was shared or absent; it now removes only its own entry. `fs.watchFile`
  also rejects a missing listener with `ERR_INVALID_ARG_TYPE` instead of returning a
  poller that could never fire.
- `fs.fstat` performed its stat synchronously and merely deferred the callback, blocking
  the loop for the whole call; it now runs on a blocking thread off an owned handle.
- `fs.statfsSync("")` reported `EINVAL` where Node reports `ENOENT`.
- Argument-validation failures out of `fs` raise a real `TypeError`, not a plain `Error`
  carrying an `ERR_*` code.
- A missing builtin export now explains itself: the error names the module, says the gap
  is oam's, and points at the tracked list, instead of only repeating V8's bare
  "does not provide an export named X".

### Changed

- `cargo run -p xtask -- conformance` gains a **builtin export-parity gate**. It runs the
  surface probe under both oam and the installed Node and fails on any missing export not
  recorded in `conformance/surface-gaps.json`, on a recorded name oam has since
  implemented (so the list can only shrink), and on an unrecorded absent module. The
  ratchet is keyed by platform because Node's own surface is; a host with no section is
  measured and reported but not gated. See `docs/node-divergences.md`.
- The macOS release leg now runs conformance. It previously ran fmt/clippy and tests
  only, so the differential corpus never executed on darwin.
- The published downloads page is regenerated and verified as part of the release path.
  Installers are byte-identical across releases, so a page two releases stale used to
  ship silently.
- Status moved from pre-alpha to **beta**, and the npm claim was corrected:
  `@yawlabs/oam` is planned but **not published**. The install script is the only
  supported channel. (Bare `oam` on npm is an unrelated project.)

## [0.9.0] - 2026-08-08

A `child_process` release. The module had no differential coverage against Node
until now, so nearly everything on a failure or option-edge path was untested —
and most of what follows was found by reading it properly for the first time
rather than by anything failing.

**Minor, not patch, because several of these change behavior you may depend on:**
`stdio` is honored at all now (`'inherit'`/`'ignore'` used to behave as
`'pipe'`); `execFile` no longer runs through a shell; `spawnSync` reports
`ENOBUFS` where it used to truncate and report success; `exec`/`execFile` now
enforce `timeout`, so a child that used to run forever gets killed; `fork` and a
misplaced `'ipc'` slot now throw; and `fs.openSync` descriptors start at 64
rather than 3.

### Added

- **oam can now BE the child of an extra-fd spawn**, not only the parent of one.
  A descriptor the parent hands us is adopted into the runtime's registry the
  first time an `fs` call names it, so `readSync(3, …)` / `writeSync(4, …)`
  behave as they do on Node instead of throwing `EBADF`, and `closeSync(4)`
  closes the *parent's* descriptor so the peer sees EOF rather than hanging.
  This was the receive half of the CDP pipe transport (divergence 19).
- **A numbered descriptor in a `stdio` slot is honored.**
  `stdio: ['ignore', logFd, logFd]` — the daemonize-into-a-logfile shape — now
  resolves the fd against the registry and hands the child a dup, so the output
  reaches the file. It used to collapse to `'inherit'`, sending the child's
  output to the parent's console while `child.stdout === null` made the
  redirect look like it had worked (divergence 18).

### Fixed

- **oam's own descriptors could collide with the parent's.** The id counter
  started at 3 — exactly where a launcher's inherited fd 3 lands — so oam's
  first `openSync` could shadow a descriptor the parent had handed it. Runtime
  descriptors now start above the inheritable window, making an unknown low fd
  unambiguously "the parent gave me this".
- **An `'ipc'` slot anywhere but last silently renumbered the child's fds.**
  oam carries the channel on a loopback socket, so the entry is spliced out of
  the array; Node instead makes that very slot the channel. The unsupportable
  positions now throw `ERR_INVALID_ARG_VALUE` naming the fix rather than
  guessing, and `fork()` enforces the same rule as `spawn()` (divergence 20).

- **`spawn()` mutated the caller's options object.** It spliced the `'ipc'`
  entry out of `options.stdio` in place, so reusing one options literal to
  spawn a pool of workers gave the first child a channel and every later one
  none — `child.send` simply undefined, with no error raised anywhere.
- **`kill()` was a silent no-op while the native handle was still resolving** —
  the window an `'ipc'` child has, because its channel must bind before exec.
  It returned `true`, `killed` stayed `false`, and the child kept running. The
  signal is now held and delivered as soon as the handle lands.
- **`exec()` accepted `timeout` and ignored it**, so the standard way to bound a
  shell-out did not bound it: a child that hung hung the caller forever. The
  callback now reports `killed: true` with the signal, as node does.
- **`execSync()`'s thrown error had no `.output`** — the 3-slot
  `[null, stdout, stderr]` array harnesses read to get both streams from one
  throw — and its message appended a newline node only appends when stderr is
  non-empty.
- **`fork()` accepted an explicit `stdio` array with no `'ipc'` entry.** Node
  throws `ERR_CHILD_PROCESS_IPC_REQUIRED`; oam built the channel anyway, which
  let code be written and tested here that is fatal the moment it runs on node.
- **`spawnSync()`'s `timeout` returned `ETIMEDOUT` without killing the child.**
  The child was moved into a worker thread that owned it, so nothing was left to
  kill: `spawnSync` returned while the child ran on holding its port, its lock
  and — now that `stdio: 'inherit'` is honored — the parent's console.
- **`spawnSync()` truncated at `maxBuffer` and reported success.** A caller got
  a short result that looked complete (a half JSON document that still parses)
  while every node-shaped check had nothing to branch on. It now reports
  `ENOBUFS` with a null status, as node does.

- **The MCP sidecar matrix ran AFTER the GitHub release went live.** Its "do not
  ship" branch fired on a build `install.sh` and `oam self-update` could already
  resolve, so the gate was reporting a verdict on something it could no longer
  stop. It now runs before the release is cut.
- **`child_process` ignored the `stdio` option entirely — `'inherit'` and
  `'ignore'` both behaved as `'pipe'`.** A child's output went into pipes the
  parent never forwarded and its stdin was a pipe nobody fed. This broke any
  launcher script that hands its own stdio to a grandchild, which is the shape
  every npm `bin` shim uses: an MCP sidecar started through one booted and then
  sat mute forever, with the launcher still reporting success. `'inherit'` is
  now a real OS-level handle hand-off, so nothing is copied through the parent.
- **`fork()` swallowed a non-silent child's output.** Node inherits stdio unless
  `silent: true`; oam piped it and dropped it, so `console.log` from a forked
  child vanished. An explicit `stdio` option on `fork()` is honored too.
- **A failed `spawn()` reported no `err.code` and used a raw JSON blob as its
  message.** It now produces node's shape — `spawn <cmd> ENOENT` with
  `.code`/`.syscall`/`.path` set — which is what `execa`, `cross-spawn` and
  every `which`-style resolver branch on.
- **A `spawn()` failure with an `'ipc'` slot hung the process forever.** The
  loopback channel kept listening because `'exit'` never fires for a child that
  never started. Relatedly, an `'ipc'` slot combined with numbered fds above 2
  silently produced a child with no IPC channel at all — that combination now
  works, but only with `'ipc'` LAST; anywhere else it throws
  `ERR_INVALID_ARG_VALUE` rather than renumbering the child's fds behind your
  back (new divergence 20).
- **A failed `spawn()` emitted no `'close'`, and a failed `fork()` still
  reported the raw native error blob.** Node follows `'error'` with `'close'`
  for a child that never started, so consumers whose completion path is
  `'close'` stalled instead of taking their error branch. Spawn errors now also
  carry `errno`, matching node.
- **`execFile()` ran its arguments through a shell.** It joined argv into one
  string and handed it to `exec()`, so arguments were re-split on whitespace and
  shell metacharacters inside an argument were executed. Node's `execFile` is
  shell-free by design and passes argv verbatim; oam's now does too.
- **Writing to a failed child's stdin killed the process.** The spawn error was
  routed into the stdin stream's error channel, where nothing listens, so
  `cp.on('error', h); cp.stdin.end(payload)` died on an uncaught error despite
  the caller handling the failure correctly.
- **`exec()`'s `maxBuffer` was enforced on stdout only, and measured
  quadratically.** stderr could grow without limit, and the stdout check
  re-concatenated everything accumulated so far on every chunk — gigabytes of
  copying at the 50MB default. Overflow now reports
  `ERR_CHILD_PROCESS_STDIO_MAXBUFFER`, as node does.
- **Smaller `child_process` parity fixes.** `exec()` no longer forwards `stdio`
  to `spawn` (node's `exec`/`execFile` deliberately own their pipes);
  `spawnSync`'s `input` no longer overrides an explicit `'ignore'`/`'inherit'`
  in slot 0 (node's docs say it does, its implementation does not); non-piped
  slots read back as `null` rather than empty buffers.

## [0.8.3] - 2026-08-07

### Added

- **`--allow-net` and `--allow-env` grants**, with env access actually enforced,
  and **`--carrier` for cross-target `oam compile`** — the compile step can now
  be handed a carrier binary for a target other than the build host.

### Fixed

- **The benchmark harness timed a binary `cargo` could replace mid-run.** `oam`
  is now staged out of `target/` before timing, so a concurrent build cannot
  swap the file underneath a measurement. This invalidated earlier published
  numbers.

## [0.8.2] - 2026-08-06

Release tooling and benchmark measurement only; no runtime behavior change.

### Fixed

- The release flow parks an in-use `oam.exe` by renaming it instead of killing
  whatever process holds it, and lands the version bump on `main` unattended.
- The Windows RSS parser no longer scrambles every reading; benchmarks were
  republished against 0.8.1 with the working parser.

## [0.8.1] - 2026-08-05

### Added

- **Node's own streams.** `require('node:stream')` is now served by a vendored
  copy of Node v22's `internal/streams` sources running over a shim prelude,
  replacing oam's in-house implementation, which was deleted.
- **Host-driven `process.nextTick`** — a FIFO queue with Node-parity exception
  handling, on an explicit V8 microtask policy with engine-owned tick points.
- **Process lifecycle parity**: `beforeExit`, the exit-code matrix, and a
  canonical uncaught-exception ladder.
- **Node eval flags** (`-e` / `-p`), `util.inspect` walker parity, and WHATWG
  `url` host rules.
- **Active-resource introspection** (`process.getActiveResourcesInfo` and
  friends), process warning flags, and `import()` from CommonJS.
- **`.tsx` / `.jsx` support** via the JSX automatic runtime.
- **`async_hooks`** init-observer registry, and `Console` write-callback
  support.
- Node's `assert` message machinery and the `util.inspect` prerequisites it
  needs; `assert.ok` now quotes the failing expression, as Node does.
- Node-shaped fatal reports, plus the `--permission`, `--env-file`, and
  `--input-type` flags, and unref'd ops.
- `child_process`: `stdio: 'ipc'` in `spawn`.
- `worker_threads`: per-worker stdout/stderr capture, `execArgv`, and the `--`
  separator. Workers inherit process flags, and `NODE_OPTIONS` is honored
  behind a strict allowlist.
- `Buffer`: pooled small allocations, and `postMessage` transfer lists.
- `structuredClone` transfer, POSIX identity APIs, and dotenv parity.
- `SECURITY.md`.

### Fixed

- `http`: `req.destroy()` now resets an in-flight response stream. It
  previously hung.
- `child_process`: `child.pid` is available synchronously from `spawn` and
  `exec`, matching Node.
- `process.env`: case-folding, own-property semantics, and inheritance by child
  processes.
- Windows: Node-shape quoting for `exec`, and bare-script invocation.
- `Buffer`: oversized `toString` throws `ERR_STRING_TOO_LONG` instead of
  crashing the process.
- `string_decoder`: rejects invalid UTF-8 lead bytes and lead-illegal second
  bytes.
- A worker that touched `worker_threads` never exited.
- The engine reports the handler's own exception, and no longer stringifies
  rejections eagerly.
- The installer gives an honest error for the unshipped `linux-arm64` target
  rather than failing obscurely.

### Changed

- Node test-suite conformance moved from 384 to 391 of 402 (97.3%); the pass
  floor was raised to match.

## [0.8.0] - 2026-07-22

Release-pipeline hardening. No runtime behavior changed.

### Added

- The install path authenticates with `GH_TOKEN`, so private-repo installs
  work.
- `NOTICE` and third-party attribution ship with the source tree.

### Changed

- Release preflight creates the release tag at `HEAD`, or re-points it if it
  already exists elsewhere, replacing a manual four-command tag dance. The
  already-published check runs before any tag mutation, so a tag whose assets
  are published is never moved.
- The CI gate fails on `THIRD_PARTY_LICENSES` drift.

### Fixed

- The release script verifies the `SHA256SUMS` manifest against the built
  artifacts before uploading anything.
- The attribution gate could never pass as written.
- CI restores conformance artifacts from `HEAD` and reports honestly when it
  discards a regenerated stamp; gate-regenerated conformance stamps are
  auto-restored.
- Three more end-to-end test waits were de-raced.

## [0.7.0] - 2026-07-11

### Added

- **Application compatibility pass**: Fastify boots, `net` reports Node's
  errno values, `ws` tears down correctly, Ed25519 is supported, `pino` writes
  to stdout as expected, and file descriptors are reserved the way Node
  reserves them.
- **Inbound OS signal delivery** — `process.on('SIGTERM' | 'SIGINT' |
  'SIGHUP')`.
- Operational hardening and the TTY raw-mode stack.
- Per-host node-suite pass floors in `xtask`, and a warn-mode `OAM-TS0000`
  diagnostic in the JSON output.

### Changed

- **CI and releases no longer use GitHub Actions.** All workflows were removed
  and replaced by a script suite: `scripts/ci-local.sh` is the gate,
  `scripts/release-local.sh` cuts releases, and cross-platform legs run on
  remote build hosts.

### Fixed

- Loader: module identity is canonicalized so an entry point and a cyclic
  re-import of it resolve to one key rather than loading twice.
- DNS: the resolver forces EDNS0 and falls back to TCP, matching Node.
- `fs`: write streams flush after each chunk, fixing a finish-before-flush
  race.
- The CLI emits an `OAM-TS0005` diagnostic when a warn-mode check misses the
  exit deadline.
- Build hosts install `tsgo` into a user npm prefix; a conformance high-water-mark
  assertion is no longer tied to a specific Node version; a phantom dev
  dependency was dropped.

## [0.6.1] - 2026-07-02

First release published with binaries and a `SHA256SUMS` manifest. Earlier tags
(`v0.2.0` through `v0.6.0`) exist in the repository but were never published as
releases.

### Added

- **Distribution**: the release pipeline and install scripts (unsigned,
  checksummed), `oam self-update` delegating to the canonical installer, and
  GitHub-authenticated installs.
- **Node test-suite conformance harness**: a vendored corpus with an exit-0
  runner, a manifest and scorecard, per-module reporting, and a skip-ratchet.
- **`node:test`** — a subset of Node's built-in test runner.
- **Node compatibility breadth**: the legacy `url.parse` / `resolve` /
  `resolveObject` API, the legacy `constants` module, `global` as a `globalThis`
  alias, the positional `readline.createInterface(stream)` form, classic
  fd-based `fs` calls (both synchronous and callback), `fs.ReadStream` and
  `fs.WriteStream` as real `Readable` / `Writable` subclasses, `zlib` streaming
  class constructors, and Node v22's stream async-iterator helpers.
- **`util.inspect` parity work**: ANSI colors, null-prototype constructor
  labeling, array subclasses, sparse holes and extra properties,
  `ArrayBuffer` / `SharedArrayBuffer` formatting, `numericSeparator`, and
  per-call options in `formatWithOptions`.
- **Resolver**: realpath-based module identity, CommonJS `.` and `..`,
  `require.resolve`, `file://` imports, and npm `package.json` subpath imports
  (`#name`).
- **Extra-fd stdio on Windows and Unix**, which is what lets oam drive Chromium
  over the CDP pipe — browser drivers work cross-platform.
- **HTTP client**: a native undici-API shim that shadows the npm package, and
  `fetch` honoring an undici dispatcher's `connect.lookup` as a real DNS pin.
- **N-API (beta)**: externals, references, wrap/class, bigint, and buffers — 22
  new symbols.
- **Pre-compilation**: a V8 bytecode code-cache for both CommonJS and ES
  modules, bytecode embedded in compiled binaries, a cache opt-out knob, and
  corruption resilience.
- **Installer**: symlink entries, a cross-process lock, and lifecycle scripts
  for trusted packages.
- **Record/replay (beta)**: `performance.now` is recorded, with an end-to-end
  determinism test.
- **`.cts` (TypeScript CommonJS) is executable.** `oam run foo.cts`, `import "./lib.cts"`
  from an ESM parent, and `require("./lib.cts")` from a CJS parent all run through the
  same oxc TS strip as `.ts` (oxc's `SourceType::from_path` resolves `.cts` as both TS
  and CJS). The previous OAM-MOD0003 gate ("write ESM TypeScript (.ts) instead") is
  gone; the OAM-MOD0003 explanation was rewritten to match. `oam test` discovery also
  covers `.cts`. *(Documented late -- this shipped in 0.6.1 but the entry was written
  under Unreleased.)*
- An opt-in `io_uring` filesystem fast path on Linux.
- macOS `os` and `process` natives via `sysinfo`.

### Performance

- Hardware SHA-256 on aarch64.
- The `fork` prewarm pool warms lazily, on first `fork()`.
- `io_uring` read chunks grow from 64 KiB to 4 MiB, fixing large-file reads.

[Unreleased]: https://github.com/YawLabs/oam/compare/v0.11.0...HEAD
[0.11.0]: https://github.com/YawLabs/oam/compare/v0.10.2...v0.11.0
[0.10.2]: https://github.com/YawLabs/oam/compare/v0.10.1...v0.10.2
[0.10.1]: https://github.com/YawLabs/oam/compare/v0.10.0...v0.10.1
[0.10.0]: https://github.com/YawLabs/oam/compare/v0.9.8...v0.10.0
[0.9.8]: https://github.com/YawLabs/oam/compare/v0.9.7...v0.9.8
[0.9.7]: https://github.com/YawLabs/oam/compare/v0.9.6...v0.9.7
[0.9.6]: https://github.com/YawLabs/oam/compare/v0.9.4...v0.9.6
[0.9.4]: https://github.com/YawLabs/oam/compare/v0.9.2...v0.9.4
[0.9.2]: https://github.com/YawLabs/oam/compare/v0.9.1...v0.9.2
[0.9.1]: https://github.com/YawLabs/oam/compare/v0.9.0...v0.9.1
[0.9.0]: https://github.com/YawLabs/oam/compare/v0.8.3...v0.9.0
[0.8.3]: https://github.com/YawLabs/oam/compare/v0.8.2...v0.8.3
[0.8.2]: https://github.com/YawLabs/oam/compare/v0.8.1...v0.8.2
[0.8.1]: https://github.com/YawLabs/oam/compare/v0.8.0...v0.8.1
[0.8.0]: https://github.com/YawLabs/oam/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/YawLabs/oam/compare/v0.6.1...v0.7.0
[0.6.1]: https://github.com/YawLabs/oam/releases/tag/v0.6.1
