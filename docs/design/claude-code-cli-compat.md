# Claude Code's JS CLI on oam: compat and startup gaps

Status: in progress, 2026-10-15. Written 2026-10-14 against oam 0.19.0 (`main` at 9c35417)
on windows-aarch64, then corrected on 2026-10-15 after a second review pass that read
the in-flight branches, the repo's actual process, and the code each gap names.
"Verified" means a command or code read confirmed it, "inferred" means it was not run.
Line numbers refer to `main` at 9c35417 unless a line says otherwise (`child.rs`,
`main.rs`, `daemon.rs` and `oam_core/src/lib.rs` are unchanged at aca6d2d; verified).

## Where the work is

A snapshot **as of 2026-10-15 05:38 (UTC-7)**, read with `gh pr view` and
`git log origin/main..<branch> --format=%s` against `origin/main` at 7128e81 (verified).
Commits are named by subject, not SHA: these branches get rebased. Gaps 1, 3 and 4b are
another session's work; check with it before touching those branches.

- **Gap 1** -- PR #410, "feat(v8): real getHeapSnapshot and writeHeapSnapshot" (open,
  branch `feat/v8-heap-snapshot`), 4 commits: the feature itself; a rustfmt of the
  permission e2e; "docs(v8): the module header no longer calls heap snapshots a stub";
  and "fix(v8): worker snapshot refusals, chunk ownership, failed-snapshot throw". It
  goes straight to Stage B (no separate Stage A).
- **Gap 3** -- PR #409, "feat(fs): native fs.watch and fs.promises.watch on OS
  notifications" (open, branch `feat/native-fs-watch`), 9 commits, among them "feat(fs):
  Linux fs.watch shares one inotify instance per run, as libuv does", a
  THIRD_PARTY_LICENSES.md regeneration for notify, conformance cases 421 and 422, and
  "test(e2e): fs_watch_detects_file_change watches a file in the temp dir", which edits
  an existing gating e2e (see "Repo process" on gate 3).
- **Gap 4b (node's compile-cache API)** -- implemented on branch `feat/compile-cache`,
  no PR yet, 8 commits from "feat(module): node's compile-cache API over oam's V8
  bytecode cache" through "test(e2e): warm runs after enableCompileCache, exit-time CJS,
  per-thread state", with conformance cases 438-439. The size cap and touch-on-hit are
  not on it (see 4b).
- **Gap 2 steps 1-2** -- four stacked branches, one commit each, PRs to be opened:
  `test/windows-std-handle-inheritance` ("test(child): a detached child does not hold
  oam's std handles, as node's does not") -> `fix/windows-std-handle-inheritance`
  ("fix(child): oam's std handles are non-inheritable from startup, as node's are") ->
  `test/stdio-inheritance-parity` ("test(child): a child does not hold oam's inherited
  extra fds, as node's does not") -> `fix/stdio-inheritance-parity` ("fix(child):
  descriptors oam inherits do not reach its children, as node's do not"). Each test
  commit lands its gating tests `#[ignore]`d, and the fix commit after it removes only
  that attribute. No `fix/help-exit-delay` branch exists.
- **Gap 4c** -- design only, at `docs/design/app-startup-snapshot.md` (draft for review).

Sign-off: the four gap 2 commits carry `Signed-off-by`; the gap 1, gap 3 and 4b
commits do not (verified by grep of each branch's commit messages).

## Why

`@anthropic-ai/claude-code@2.1.112` is the last npm release that ships a plain Node
`cli.js` (13.7 MB ESM bundle); 2.1.113+ ship only a Bun `--compile` binary, which no
V8 runtime can host (Bun-only APIs, JSC bytecode, `require()` of ESM in cycles). That
cli.js is still a good real-world conformance target: one large, dependency-heavy,
long-running TUI app. Running it surfaced the gaps below. Each is worth fixing for
oam's own sake (dev servers, watchers, CLIs that spawn daemons, large-bundle startup),
not only for Claude Code.

**Non-goal:** running current Claude Code (Bun binary) on oam. That would need a Bun
compatibility layer tracking a near-daily release train.

**Not a performance fix for Claude Code users.** The native build already ships
precompiled code. Do not market any of this as a Claude Code speedup.

## Reproduce

```bash
mkdir -p /tmp/cc && cd /tmp/cc
npm pack @anthropic-ai/claude-code@2.1.112 && tar xzf anthropic-ai-claude-code-2.1.112.tgz
oam run --no-check package/cli.js -- --version    # fails on 0.19.0: gap 1
```

Note the `--`. `script_args` is `#[arg(last = true)]` (`crates/oam_cli/src/main.rs:79-81`),
so after `oam run file`, clap owns every flag until `--`. Verified with oam 0.19.0 on a
script that prints its argv:

```
oam run tiny.js --help        # prints oam's own `run` help, exit 0 -- the script never runs
oam run tiny.js -h            # the same, short form
oam run tiny.js --version     # error: unexpected argument '--version' found, exit 2
oam run tiny.js -- --help     # SCRIPT RAN ["--help"], exit 0
oam tiny.js --help            # SCRIPT RAN ["--help"], exit 0 (bare form, as node)
```

The silent `--help` case is what produced a false "exit delay" report (gap 2): the
"linger" was timed on oam's own help, not on cli.js. `--version` is not silent; it
errors like any other unknown flag. Divergence #10 in `docs/node-divergences.md` now
records the `--help` case.

With gap 1 stubbed by hand (rewrite the `v8` named import at cli.js:7059 to a namespace
import), `--version`, `--help`, `-p "reply with the word ok"` and the full TUI in a
120x40 ConPTY all work, and `--help` output is byte-identical to node's.

cli.js imports only `createRequire` from `node:module` / `module` (four sites) and never
names `enableCompileCache` (verified by grep of cli.js 2.1.112), so gap 4b's missing
`node:module` names are not a second link blocker for it.

Baseline (oam 0.19.0 / node 22.22.2 / bun 1.3.14, all native arm64, loaded host --
treat timings as rough):

| | oam | node | bun |
|---|---|---|---|
| `--version` peak working set | 89 MB | 178 MB | 164-168 MB |
| Idle TUI CPU | 57-109 ms/s | 9-21 ms/s | 14 ms/s |
| Idle TUI working set | 216-221 MB | 144-146 MB | 282 MB |
| TUI prompt rendered | 3.0-3.6 s | 2.9-4.0 s | 4.5 s |

Native `claude.exe` 2.1.300 for reference: prompt 1.4 s, idle 24 ms/s, 260 MB.

## Repo process (applies to every gap)

From CONTRIBUTING.md, AI-POLICY.md, RELIABILITY.md, `scripts/ci-local.sh`, the xtask
gates and recent git history (verified unless marked):

- **Test-first: the written rule and the practice differ; get one maintainer ruling.**
  AI-POLICY.md:15-17 says behavior specs are reviewed and merged BEFORE the
  implementation and that implementation PRs may not modify the tests that gate them.
  In practice, recent fix PRs land a NEW gating test in the same PR as the fix and say
  in the body that it fails on `main` (e.g. #402 / 4818156 adds case 412: "Run against
  main's once, it fails on the first six lines"; #400 adds case 411, #396 case 410).
  Gate 3 has been applied to tests that already exist. The in-flight branches also mix
  tests and fixes in one commit, and PR #409 edits an existing gating e2e. Ask which
  reading applies before splitting anything into spec PRs.
- **No expected-fail mechanism for conformance cases.** The gate runs every
  `conformance/cases/*.mjs` (`xtask/src/conformance.rs:249-255`) and fails on any case
  that diverges from node (`:639-644`); the only exception is both runtimes hitting
  the case's own watchdog. A failing spec case therefore cannot merge ahead of its fix:
  it lands in the same PR as the fix. The only expected-fail lists are the node-suite
  manifest skips under ceilings (`node_suite.rs:409-458`) and `surface-gaps.json` for
  export names.
- **`#[ignore]`d e2e as a contract has one precedent.** PR #311 (d55ea86) landed an
  `#[ignore = "..."]` e2e with its measurements (the #155 freshness gate does not hold
  over TLS) as "the contract for that fix", and #355 (fdeb434) fixed it and removed
  the ignore. `e2e.rs` on `main` has no such test today. The gap 2 branches follow the
  same shape: each test branch adds `#[ignore = "..."]` gating tests in new files
  (`stdio_inheritance.rs`, `stdio_inheritance_fds.rs`), and the fix branch above it
  removes only the attribute. Whether un-ignoring counts as "modifying the gating test"
  is part of the ruling above.
- **Provenance is a trailer, not a label.** There is no agent-provenance GitHub label
  (only the 9 defaults). Agent work carries `Co-authored-by: Claude ...
  <noreply@anthropic.com>` and a "Generated with Claude Code" PR-body footer.
- **DCO is documented but rarely present.** CONTRIBUTING.md:8 and the PR template ask
  for `git commit -s`; 13 `Signed-off-by` lines appear in the last 300 commits, none in
  the recent fix commits. Sign off anyway.
- ~400 logic lines per PR; NOTICE attribution for ported logic; two-human review for
  public-API additions -- that covers gap 1's new `v8` name, gap 3's `fs/promises.watch`,
  and gap 4b's `module.*` compile-cache API.
- **The unsafe ratchet is bidirectional.** `conformance/unsafe-budget.json` (oam_core
  66, oam_engine 605) fails a count ABOVE its ceiling and also one BELOW it until it is
  re-blessed with `cargo run -p xtask -- unsafe-budget --regen` (ci-local.sh:775-788).
  New unsafe must be argued in the PR; a PR that removes unsafe commits the regenerated
  json.
- **`conformance/surface-gaps.json` is a ratchet per host.** The gate fails on a new
  missing name AND on a listed name oam now provides (`conformance.rs:715-733`).
  `scripts/gen-surface-gaps.mjs` regenerates only the CURRENT host's section; from a
  Windows box the linux and darwin sections are edited by hand and only verified on the
  remote release legs. The present/missing counts are not gated but should be kept
  right by hand.
- **CHANGELOG section: the untagged `## [0.19.1]`.** `## [Unreleased]` stays empty;
  post-0.19.0 changes go into `## [0.19.1] - 2026-10-13` (#344/#345, the #285 readdir
  fixes), and no v0.19.1 tag exists yet (only v0.19.0; verified). PRs #409 and #410,
  `feat/compile-cache` and the gap 2 fix branches all write there; keep it that way.
- Windows arm64 is tier-1. Node differential pinned to `.node-version` (22.22.2).
- Timing assertions: relative (EOF vs exit, warm vs cold), generous margins. The
  reference host showed 10x noise; never pin absolute durations.
- Conformance case numbers are contended: `main` ends at 416, and the in-flight
  branches use 417, 418, 421, 422, 425 and 430-442. Coordinate before picking one.
- Parallel builds: a per-agent `CARGO_TARGET_DIR=<repo>/target-agents-<x>`; neither
  `gc-target.sh` nor `ci-local.sh` follows CARGO_TARGET_DIR, so under one, run the
  ci-local steps individually as recent PR bodies do.

## Gap 1 -- `v8.getHeapSnapshot` missing breaks module LINKING

Root cause (verified): oam's `v8` builtin is a hand-written JS stub
(`js/node_compat.js:32843-32913`, "Real V8 bindings land with the diagnostics wave").
Its export object has no `getHeapSnapshot`. Builtin ESM named exports come from the
module object's own keys (`crates/oam_engine/src/cjs.rs:1044-1074`), so
`import {getHeapSnapshot} from "v8"` is a LINK error that kills the whole graph.
**Verified: it is the ONLY missing builtin named import in cli.js 2.1.112.** Adjacent
fake-success stubs: `writeHeapSnapshot()` returns `""` and writes nothing (:32893),
`getHeapSpaceStatistics()` returns `[]` (:32888), `getHeapCodeStatistics()` all zeros
(:32889-32891), `Worker.prototype.getHeapSnapshot()` resolves an empty `Uint8Array`
(:41403).

The engine already supports it: rusty_v8 150.0.0 (Cargo.lock:4053) has safe
`Isolate::take_heap_snapshot(FnMut(&[u8]) -> bool)` (it returns `()`; a failed
snapshot simply never calls back) and `number_of_heap_spaces` /
`get_heap_space_statistics`. It has NO `get_heap_code_statistics`.

**Stage A -- link unblock (0.5 day).** Export `getHeapSnapshot` that validates options
like node, then throws `ERR_METHOD_NOT_IMPLEMENTED` (codes table at node_compat.js:1063).
Make `writeHeapSnapshot` throw the same instead of returning `""`. Tests assert `.code`
only: oam's codes table builds a TypeError where node's is a plain Error (inferred).
The branch skipped this and went straight to Stage B, which is fine; Stage A stays the
fallback if Stage B review stalls.

**Stage B -- real implementation (2.5-3 days).** Branch status per step in brackets
(from a code read of PR #410's branch at the snapshot time above).
1. Native snapshot op next to `heapStatistics` (`node_ops.rs:534`), calling
   `take_heap_snapshot`. V8's OutputStream default chunk size is 1 KiB (`v8-profiler.h`
   `GetChunkSize` returns 1024; the binding does not override it), so do not make one
   ArrayBuffer per V8 chunk (~4,700 for a 4.8 MB snapshot). [PR #410: gathers the whole
   snapshot into one Vec and, since its "worker snapshot refusals, chunk ownership,
   failed-snapshot throw" commit, hands out each 64 KiB stream chunk as its own copy, so
   a kept chunk does not pin the whole snapshot.] A failed snapshot (no callback) ->
   throw, never an empty stream. [Done in the same commit.]
2. Native `heapSpaceStatistics()` with node's field names. [Not done: PR #410's
   `getHeapSpaceStatistics()` still returns `[]`.]
3. JS `getHeapSnapshot()`: snapshot at call time, return a `Readable` with node's
   `HeapSnapshotStream` high-water mark, which is the platform default:
   **16 KiB on win32, 64 KiB elsewhere** (`js/vendor/node-streams/internal/streams/state.js:20`;
   the first draft said 16384, true only on Windows). Compare against node in the test
   rather than pinning a number. [Branch takes the platform default.]
4. JS `writeHeapSnapshot(filename)` must not bypass `--allow-fs-write`. The first draft
   required a JS implementation over `fs.openSync/writeSync/closeSync` for that reason.
   The branch writes from a Rust op instead and still gates it: a named path goes
   through `fs_path_of` -> `check_write_path`, the gate `fs.open` uses (`fs_path_of`
   and `open_path_of` in the branch's `node_ops.rs`), and the generated name checks the
   cwd (code read, not run). It also streams to disk without holding the whole
   snapshot. Either route is acceptable if the permission e2e covers it. Default name
   `Heap.YYYYMMDD.HHMMSS.<pid>.<threadId>.<seq:3>.heapsnapshot` in local time; resolve
   relative names against `process.cwd()`; return the name as given. [Done.]
5. `Worker.getHeapSnapshot()`: reject with `ERR_METHOD_NOT_IMPLEMENTED` (or
   `ERR_WORKER_NOT_RUNNING` before online / after exit, as node) until a cross-isolate
   version lands (+1 day). Stop resolving an empty `Uint8Array`. [Done.]
6. `exposeInternals` / `exposeNumericValues`: validated, not honored (the binding takes
   no options). Record as a divergence ROW in `docs/node-divergences.md` and file the
   upstream rusty_v8 issue; do not vendor V8. [PR #410 adds a `node:v8` heap snapshots
   row to `docs/node-divergences.md` that records this; whether the upstream issue is
   filed was not checked.]

Tests: cli.js's exact import line links -- the bare `from "v8"` specifier, not only
`node:v8` (branch case 425 uses `node:v8`); `pipeline(getHeapSnapshot(),
createWriteStream)` produces parseable JSON containing a retained marker class name;
Readable + Buffer chunks; bad-arg codes; default filename regex and seq increment;
ENOENT for a missing dir (assert code + syscall, NOT errno -- it is -4058 on Windows,
-2 on POSIX); a denial under restrictive `--allow-fs-write`; non-empty space stats (do
not pin count or names: node 22 is V8 12, oam is V8 15). Keep a running Worker's
snapshot out of the node differential (node 22.22.2 segfaulted at exit in the
skeptic's repro); case 425 only calls it on a worker that is not running, which
probably avoids that (not run).

Policy to write down in `docs/node-divergences.md` "Builtin export names" (now :3837 on
aca6d2d): "never fake success; a name that must exist to LINK may be exported and refuse
loudly at use", applied **per name after triage**, never as a blanket auto-stub (it
would flip feature detection and empty the surface-gaps honesty metric). Update that
section's measured table from surface-gaps.json in the same change. PR #410 updates
the table's `v8` row (9 -> 8 names); the policy text is not on it. Consider a `refused`
list in surface-gaps.json.

Triage of the other 8 missing v8 names (`surface-gaps.json` lists 9 per host, e.g.
win32 :419-423; +1.5-2 days): implement `isStringOneByteRepresentation`; refuse
`queryObjects`, `GCProfiler`, `getCppHeapStatistics`; decide `promiseHooks` and
`DefaultSerializer/Deserializer` separately (oam's Serializer is JSON, not V8 wire
format); and **`setHeapSnapshotNearHeapLimit`**, which the first draft left out --
probably a refusal, or an implementation over V8's near-heap-limit callback (whether
rusty_v8 150 exposes one is not checked).

## Gap 2 -- reframed: the `--help` "linger" is unreproduced; the real bug is a
## Windows detached-child stdio leak

The original report (~2.5 s between `--help` output and exit) **did not reproduce**
once the flag reached the script: oam exits 10-20 ms after its last stdout byte, same
as node or faster, in both piped and real-console runs. Likely sources: host load (node
`--help` itself ranged 1.5-26 s on the same host) and the missing `--` above (now
verified: without it, oam printed its own `run` help). Treat it as UNREPRODUCED, not
refuted: re-measure once in a real ConPTY pane on an idle host before closing it. No
one owns that re-measure yet.

**The verified bug found on the way:** on Windows, `spawn_child`
(`crates/oam_core/src/child.rs:138`, spawning at :236) uses tokio/std `Command`, i.e.
`CreateProcessW(bInheritHandles=TRUE)` with no handle list, so every child inherits
oam's inheritable std handles. Non-detached children hide it: they are adopted into the
kill-on-close job (`if !detached` at :240-242). `detached: true` children skip the job
and get `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP` (`windows_creation_flags`,
:415-424; the contract is described in the doc comment at :133-136), and **keep the
parent's stdout pipe open after oam exits**, so `$(oam run ...)`, `| tail` and CI log
capture wait for the grandchild. `spawn_sync` (:427, std `Command`, `if !detached` at
:572) has the same exposure. `child_win::spawn_extra` already uses
`PROC_THREAD_ATTRIBUTE_HANDLE_LIST` and does not leak. Repro: spawn
`PING -n 4 127.0.0.1` with `{detached:true, stdio:'ignore'}`, `.unref()`, print,
`process.exit(0)`, capture stdout from a parent: oam exit 541 ms / pipe close 3502 ms;
node exit == close. Claude Code hits this path: its Bash-tool shell provider, editor
launcher and terminal launcher spawn detached (cli.js:3556, :5771, :18100).

oam already has the fix, applied only on the tsgo and MCP paths:
`oam_ts::daemon::unshare_std_handles()` (`crates/oam_ts/src/daemon.rs:1010-1033`, a
no-op off Windows; called at `oam_ts/src/lib.rs:311`, `oam_mcp/src/lib.rs:296` and
`daemon.rs:1053`). It clears only `HANDLE_FLAG_INHERIT` on the three std handles and is
the whole of oam_ts's unsafe budget.

Plan:
1. **Step 1 (1-1.5 days):** call `oam_ts::daemon::unshare_std_handles()` once at startup
   in `crates/oam_cli/src/main.rs`, right after `snapshot_inherited_fds()` (:277) and
   before the `extract_embedded()` early return (:288) so compiled single-exe binaries
   get it too. Zero new unsafe: oam_cli already depends on oam_ts (Cargo.toml:28), and
   `windows-sys` is only a dev-dependency of oam_cli, so do not copy the function there.
   Keep the per-site calls (library embedders skip main.rs). Clearing the bit does not
   break oam's own output or `stdio:'inherit'` children: oam looks up std handles per
   use, and std's `Stdio::inherit` and child_win's Inherit arm both duplicate the
   handle as a new inheritable one (`child_win.rs:311-352`; code read, std source not
   re-read). Scope the CHANGELOG claim to "Windows std handles".
2. **Step 2 -- real `uv_disable_stdio_inheritance` parity (total 2.5-3.5 days):**
   unsharing the three std handles is not the whole of node's behavior (inferred from
   libuv): also clear inheritance on CRT fds passed in `STARTUPINFO.lpReserved2` (e.g.
   a node parent's IPC pipe, which oam snapshots at `oam_core/src/lib.rs:929-952` but
   leaves inheritable), and on unix set `FD_CLOEXEC` on inherited fds.
   - It belongs beside `snapshot_inherited_fds` in oam_core. oam_ts cannot delegate to
     it (oam_ts depends only on oam_diagnostics; pulling in oam_core would drag tokio
     into the type-checker crate), so two copies stay: oam_ts's three-handle version for
     library embedders and oam_core's broader one for the binary.
   - Unix: **fds >= 3 only**, and say why in a comment. Copying libuv's loop from fd 0
     would close every `stdio:'inherit'` child's stdio at exec, because Rust std's
     `Stdio::inherit` does no `dup2` for fds 0-2 (libuv's loop is safe only because
     libuv always dup2s; from libuv knowledge, not re-checked). `child_unix.rs` moves
     sources to high fds with `F_DUPFD_CLOEXEC` before `dup2`, so it never needs a
     cleared CLOEXEC.
   - Coverage is bounded by `MAX_INHERITED_FD` (63, `oam_core/src/lib.rs:716`): fds of
     64 and up keep inheriting, unlike libuv on Windows, which walks the whole
     `lpReserved2` count. Document that under divergence #19 or parse `lpReserved2`
     (more unsafe).
   - Unsafe: Windows one block (collect handles through the safe `crt_fd::osfhandle`,
     then `GetStdHandle` x3 + `SetHandleInformation` in one block); unix one block for
     `fcntl` F_GETFD/F_SETFD. oam_core 66 -> 68, argued in the PR, with the json
     regenerated. [`fix/stdio-inheritance-parity` lands at 66 -> 67, json
     regenerated.] Folding F_SETFD into `probe_open`'s existing block would keep unix at
     +0 but gives the snapshot a side effect embedders would not expect; recommended
     against.
3. **Follow-up, separate design:** `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` in
   `spawn_child` (moving async spawns onto the child_win `CreateProcessW` path), which
   closes leaks of all other inheritable handles. 3-5 days, >400 lines, needs
   decomposition.

Tests (gating, Windows e2e): with a detached grandchild alive, stdout EOF arrives while
the grandchild's alive flag still exists -- an ordering oracle, no absolute timing
(reuse `oam_cli/tests/child_job.rs` and the e2e.rs:16449-16545 pattern: ReleaseOnDrop,
alive/stop flag files); regression that `stdio:'inherit'` children still write to
oam's stdout after the unshare (child_win duplicates the handle, `child_win.rs:315-335`);
detached `stdio:'pipe'` still delivers data. Step 2 adds a Windows fork-IPC test with a
NODE parent (the `lpReserved2` IPC pipe; an oam-parent extra-fd test does not cover it)
and a POSIX fd-3 EOF test. Do NOT add a node-differential fixture for this: stdout + exit
code already match today, so it gates nothing.

Doc follow-up (no code): divergence #10 ("`oam run file.js` needs `--` before script
flags", `docs/node-divergences.md:808-818`) already existed; the first draft asked for a
new note. What was missing is the silent `--help` case; it is added on this branch.
Changing the contract (clap `trailing_var_arg`) is a CLI-contract change for its own
discussion.

## Gap 3 -- `fs.watch` is a 100 ms stat poll

Root cause on `main` (verified by code read; CPU link inferred, see below):
`js/node_compat.js:13415-13470` `fsWatch` is a `setInterval` (default 100 ms; also reads
a non-node `opts.interval`, :13445) calling `natives.fsStat` and comparing `mtimeMs`.
- **CPU:** ~10 async stats/s per watcher; Claude Code's ~63 watchers (chokidar v4,
  pure `fs.watch` per path, depth 2) -> ~630 op wakes/s. Arithmetic fits the measured
  57-109 ms/s, but the dedicated repro has not run: **measure first**, including a
  0-watcher baseline on both runtimes. Still open; the branch plans the perf receipt
  only after landing.
- **Liveness:** `ref()/unref()` are `return watcher` no-ops (:13467-13468) and
  `persistent:false` is ignored, so an unclosed watcher pins the process forever.
- **Semantics:** directory mtime does not change on child content writes (missed);
  every event reports the watched path's own basename (:13451-13456); always
  `'change'`, never `'rename'`; `recursive`, `encoding`, `signal` ignored; no
  `fs.FSWatcher`; no `fs/promises.watch` (`docs/node-divergences.md:3861`, :3866).
  `close()` emits `'close'` synchronously on every call (node: once, on nextTick).
- No native watcher anywhere in crates/ on `main`, nothing in Cargo.lock (no notify,
  inotify, kqueue, fsevent-sys).

`fs.watchFile` (:13472-13560) is stat polling in node too. Leave it.

Design (PR #409 divergences noted in brackets; from a code read of its branch at the
snapshot time above):
- **Backend:** `notify` 8.2 in `crates/oam_core/src/fs_watch*` (safe API, so oam_core's
  unsafe ceiling holds -- only if the backend and the node_ops glue stay safe; say so
  in the PR). Windows `ReadDirectoryChangesW` (file watches watch the parent and
  filter, as libuv does); Linux inotify. The first draft wanted one lazily created
  watcher per CoreRuntime, demultiplexed by path. [PR #409: one notify watcher per
  `fs.watch` -- one OS thread per watcher -- except Linux, which shares one inotify
  instance per run through the `inotify` crate directly, as libuv shares one
  per loop ("Linux fs.watch shares one inotify instance per run"). It also vendors
  notify 8.2.0 with Windows fixes.]
- **macOS: kqueue for single-file watches, FSEvents only for directories** (libuv's
  split). FSEvents accumulates flags (`ItemCreated|ItemModified` on a write to a
  recently created file) which maps to `'rename'` and would break the existing gating
  test `crates/oam_cli/tests/e2e.rs:19573-19602` (`fs_watch_detects_file_change`,
  asserts `change` at :19596). **notify cannot supply both in one build** (verified,
  notify-8.2.0): the default feature `macos_fsevent` and the opt-in `macos_kqueue` are
  alternatives -- `FsEventWatcher` is compiled only under
  `all(target_os = "macos", not(feature = "macos_kqueue"))` and `KqueueWatcher` on
  macOS only under `feature = "macos_kqueue"` (`src/lib.rs:186-205`). So the file-watch
  path on macOS needs the `kqueue` crate directly (1.2.1, MIT, a safe API --
  `Watcher::add_filename` / `poll`; verified; already in the branch's Cargo.lock as a
  notify dependency for the BSDs) beside notify's FSEvents for directories.
  `rustix::event::kqueue` is the other route, but its `kevent` is an `unsafe fn`.
  [PR #409: FSEvents for files too; its "test(e2e): fs_watch_detects_file_change
  watches a file in the temp dir" commit edits the gating e2e, which the gate-3 ruling
  has to cover.]
- FSEvents stream restarts on every add: notify 8.2 has the batching API
  (`Watcher::paths_mut()` returning a `PathsMut` with `commit()`, `src/lib.rs:310`,
  `:368`; FSEvents implements it, `src/fsevent.rs:286`, `:589`; verified). Moot under
  the branch's per-watcher design, needed if a shared watcher returns.
- **Event mapping to node:** Create / Remove / Modify(Name) -> `'rename'` (a Windows
  rename emits old name then new, as libuv); Modify(Data|Metadata) -> `'change'`;
  filename relative to the watched path, platform separator for recursive. Corrected
  from the first draft: do NOT map Close(Write) to `'change'` -- libuv's inotify mask
  has no `IN_CLOSE_WRITE` (from libuv knowledge, not re-checked); the branch drops all
  Access kinds. Overflow / need_rescan: the first draft said `('change', null)`, the
  branch emits `('rename', null)` citing libuv Windows; neither is verified against
  libuv's source -- check before the spec.
- **Linux inode semantics, keep node's:** replace-by-rename (editor temp file renamed
  over the original) reports `'rename'` on a file watcher and the watch then dies.
  Windows and macOS keep working. Do not make Linux "nicer" than node.
- **Plumbing:** the existing parked-read pattern, no new wake mechanism.
  `HandleKey::FsWatch(u64)` (enum at `oam_core/src/lib.rs:1023`); natives
  `fsWatchStart(path, recursive)` (throws node's sync ENOENT shape, keep conformance
  case 378 green), `fsWatchNext(id)` via `spawn_handle_op` draining all buffered events
  into one settle, `fsWatchSetRef(id, bool)` via `set_handle_ref`, `fsWatchClose(id)`.
  Model on `op_tcp_read` / `op_tcp_set_ref` (`node_ops.rs:5038`, :5137). An idle watcher
  is one parked future: ~0 CPU. `unref()` / `persistent:false` ->
  `set_handle_ref(false)`. [Branch does this.]
- **JS:** `FSWatcher` class (EventEmitter) with node's option validation, `encoding`
  ('buffer' -> Buffer filename), `signal` (pre-aborted closes on nextTick), idempotent
  `close()` emitting `'close'` once on nextTick (the existing e2e at
  `e2e.rs:19581-19590` calls close twice), native errors -> `'error'`. Do NOT export
  `fs.FSWatcher`: the first draft asked for it, but node v22.22.2's `fs` has no such
  property (verified: `typeof require('fs').FSWatcher` is `'undefined'`), and PR #409
  keeps the class internal, as node does. `fs/promises.watch` as an async generator
  over the same native, with `maxQueue` / `overflow` [done].
- **Windows, watch ended by the OS:** the first draft expected EPERM as `'error'` for
  a deleted watched directory (chokidar v4 handles it). PR #409 (its "Windows watch ends
  are reported" commit, with a patch to the vendored notify) now reports node's
  `\\?\` `'rename'` once for a deleted watched directory, and EPERM as `'error'` for a
  watched file whose directory is deleted, per its own node measurements (not re-run
  here). Test both cases against node on Windows.

Rejected: cheaper polling (still O(watchers), still no child filenames). Hand-rolled
backends: zero new crates, but Windows overlapped/IOCP parsing and macOS FSEvents FFI
add unsafe against a ratchet, and the rustix route covers unix only -- keep as the
fallback if the notify dependency is vetoed (+3-5 days).

Optional **PR 0 stopgap (0.5-1 day)** if idle CPU is needed before the real work: fold
all watchers into ONE shared ref-counted timer with a batched native stat (one settle
per tick), and honor `unref` / `persistent:false` on that timer. No new crates. Fixes
none of the semantic gaps. Gain unmeasured. With the native branch this far along,
probably drop it; nobody has decided.

Tests: Rust unit tests for the event mapping, relative filenames, two subscribers on
one path (closing one leaves the other delivering), close resolving the pending next;
ref accounting tests; set-normalised node-differential cases per step (create, append,
rename, unlink, mkdir; file watch; buffer encoding; pre-aborted signal; idempotent
close; option-validation codes); `fs/promises.watch` (iterate/break, abort, overflow);
recursive on all three OSes including a subdir created after the watch; unref'd /
non-persistent watcher exits promptly; Windows rename old->new ordering; Windows
watched-dir delete (per the re-measure); Linux replace-by-rename; Linux ENOSPC on
`max_user_watches` surfacing node's error shape; watched path inside a deleted dir per
OS; a Worker that creates a watcher and exits (no leaked notify thread/handles on
CoreRuntime drop). A unit test that writes immediately after start can flake on macOS,
where FSEvents starts asynchronously; retry or wait for readiness. Existing
`e2e.rs:19573`, the watchFile e2e tests (`:33503-33604` on 9c35417, shifted by the
branch) and conformance case 378 stay green. Perf receipt: 63 idle directory watchers
for 10 s, oam vs node, on all three hosts; target oam <= 1.5x node. Chokidar v4 smoke
(depth 2, `awaitWriteFinish {stabilityThreshold:500, pollInterval:200}`) matching
node's add/change/unlink sequence.

PR split (5 under the line cap): tests; Rust backend + natives + dependency + licenses;
JS FSWatcher; `fs/promises.watch` (the surface-gaps removal of fs/promises `watch`
rides with THIS PR, not the docs PR -- the ratchet fails once the name exists but is
still listed; fix the present/missing counts too, the branch does not); docs (remove
the divergence rows, ADD a residual macOS coalescing entry, CHANGELOG). PR #409 is open
as one PR (9 commits, about 7,600 added lines, most of them the vendored notify 8.2.0);
the split and the gate-3 question go to the maintainer together.

Risks: new supply chain (notify + inotify/kqueue/fsevent-sys/notify-types; notify is
CC0-1.0, needs about.toml; THIRD_PARTY_LICENSES.md regenerated or ci-local step 10
fails), Linux ENOSPC on `max_user_watches` must surface as node's error, Windows open
dir handles block deleting the watched dir's parent (as libuv), ReadDirectoryChangesW
unreliable on some SMB paths and silent on WSL's `/mnt/c` (the branch measured node
silent there too), per-watcher ordering across the batch hop.

## Gap 4 -- startup: prompt at 3.0-3.6 s vs 1.4 s native

On the same cli.js oam already matches node and beats bun, so the gap to 1.4 s is not an
oam regression.

**Correction to the first draft.** It said "a runtime handed cli.js parses and compiles
13.7 MB every start". That is false for oam: oam already has a persistent V8 bytecode
cache, **on by default**, for ESM (static and dynamic import) and CJS user modules
(`crates/oam_engine/src/code_cache.rs`; verified by code read and by the 4a run below).
- Keyed by content: sha256 of V8's `cached_data_version_tag`, a format version, the
  module kind, CJS wrapper params and the source (`code_cache.rs:190-208`).
- Atomic writes (temp file + rename, `:338-368`); a blob V8 rejects is recompiled and
  refreshed silently (`modules.rs:2265-2291`, `cjs.rs:701-717`, `:770-775`).
- Per-OS dirs: `%LOCALAPPDATA%\oam\bytecode`, `$XDG_CACHE_HOME/oam/bytecode` or
  `~/.cache/oam/bytecode`, falling back to temp, never the cwd (`:146-161`).
- ESM bytecode is produced AFTER evaluation, so it also carries the functions top-level
  evaluation compiled (`modules.rs:1847-1878`).
- `OAM_CODE_CACHE=0` turns it off (`:62-70`); `oam cache info|clean`; `oam compile`
  seeds embedded bytecode; e2e coverage at `e2e.rs:31889-32335`.
- Eviction is age-only: once a day a detached thread deletes blobs older than 30 days
  (`:370-489`). There is no size cap, and a hit never refreshes a blob's mtime, so a
  blob used daily is still evicted at 30 days (a documented trade, `:380-384`).

So the 3.0-3.6 s oam baseline was almost certainly a warm-cache number, and node's
`NODE_COMPILE_CACHE` cold-warm delta is not headroom a new cache could add. The other
likely part of the gap to 1.4 s stands (inferred, not profiled): 2.1.300 is ~2,400
code-split chunks loaded on demand and 190 releases newer; it likely renders the prompt
before MCP servers, hooks and git finish starting. No runtime change recovers that.

### 4a. Measured (2026-10-15)

Host: Windows 11 arm64, 12 cores, heavily loaded (see load probes). oam 0.19.0 (installed
`~/.oam/bin/oam.exe`), node v22.22.2, bun 1.3.14, cli.js 2.1.112 (13,711,684 bytes). oam
ran `cli-patched.js`, which differs from cli.js only in the `v8` import (`getHeapSnapshot`
moved from a named import to a namespace import + destructure; +41 bytes) because 0.19.0
cannot link the named import. node and bun ran the unmodified cli.js.

**Bottom line.**
- **The 4b premise does not hold for oam.** oam already ships a persistent bytecode cache
  (`crates/oam_engine/src/code_cache.rs`). It is on by default and its blobs are produced
  after evaluation, so they also carry the functions evaluation compiled. It already gives
  the cold-to-warm drop 4b was meant to deliver: `--version` falls from 578 ms to 230 ms
  (medians), about 2.5x. Warm oam also beats node with a warm `NODE_COMPILE_CACHE`:
  about 3x on `--version` (230 vs 683 ms) and about 1.5x on `--help` (832 vs 1230 ms) in
  the interleaved series. The earlier non-interleaved series did not reproduce the
  `--help` gap (oam warm 998 vs node warm 931 ms); on `--version` it showed about 1.8x
  (336 vs 599 ms).
- **The doc's Gap 4 wording is wrong for oam.** It says "a runtime handed cli.js parses
  and compiles 13.7 MB every start". That is true only on the very first run, or with
  `OAM_CODE_CACHE=0`.
- **Parse/compile is not where the 3 s goes.** On a warm cache, oam spends ~180-320 ms
  before cli.js's first statement and ~13 ms evaluating `--version`. Even with the cache
  off, it is ~520 ms. So the 3.0-3.6 s TUI-prompt time is almost all top-level app work
  and waiting after module load, which no compile cache can touch. 4c (startup snapshot)
  can recover at most the ~150-250 ms of warm pre-eval time (boot ~40-50 ms is already
  paid) plus the app-init part a snapshot can capture.
- **What 4b leaves open is API parity, not speed.** At the time of the measurement oam
  (0.19.0, and `main`) had no `module.enableCompileCache()`, `module.getCompileCacheDir()`,
  `module.flushCompileCache()` or `NODE_COMPILE_CACHE`. Branch `feat/compile-cache`
  implements them since (see 4b).

**Cache artefacts.**
- A fresh `OAM_CACHE_DIR` after one cli.js run holds 33 blobs, 4,320,376 bytes: the
  cli.js blob is 4,245,776 bytes, and the other 32 are small builtin-module blobs.
  `--version` and `--help` produced the same-size blob.
- node's `NODE_COMPILE_CACHE` blob for cli.js is ~4,072,300 bytes.
- The default user cache (`oam cache info`) is at 13,077 entries and 523,170,264 bytes. It
  is shared by every oam run on the box, so it is not specific to this test.

#### Wall clock, interleaved (9 rounds; every config once per round in rotated order; ms)

| config | median | min | max |
|---|---:|---:|---:|
| load probe `node -e 0` (once per round) | 177 | 109 | 786 |
| `oam run empty.mjs`, cache off | 56 | 40 | 445 |
| `oam run empty.mjs`, warm | 146 | 38 | 347 |
| `bun -e 0` | 229 | 149 | 1185 |
| `--version` (a) oam `OAM_CODE_CACHE=0` | 578 | 449 | 3331 |
| `--version` (b) oam fresh `OAM_CACHE_DIR` (cold + produce) | 946 | 554 | 1400 |
| `--version` (c) oam warm dir | **230** | **185** | 861 |
| `--version` (d) node cold | 1142 | 690 | 1510 |
| `--version` (d2) node fresh `NODE_COMPILE_CACHE` (cold + produce) | 1063 | 742 | 1725 |
| `--version` (e) node `NODE_COMPILE_CACHE` warm | 683 | 388 | 1435 |
| `--version` (f) bun | 847 | 553 | 2037 |
| `--help` (a) oam cache off | 1042 | 674 | 2084 |
| `--help` (b) oam cold + produce | 1277 | 808 | 2417 |
| `--help` (c) oam warm | **832** | **484** | 1623 |
| `--help` (d) node cold | 1487 | 1184 | 4363 |
| `--help` (d2) node cold + produce | 1599 | 1099 | 5748 |
| `--help` (e) node warm | 1230 | 764 | 2580 |
| `--help` (f) bun | 1451 | 737 | 4455 |

**How to read this.** On a quiet arm64 box, `node -e 0` is roughly 40-60 ms (inferred, not
measured here). It ran at 109-786 ms in this series, so the host was 2-10x degraded.
Rounds 2, 5 and 8 were the heavy ones and inflate every config about equally, which is
why the min column is the most trustworthy number. An earlier non-interleaved series (7
runs each, `bench-run1.txt`; medians) agrees on `--version` that oam warm < node warm <
node cold (336 < 599 < 965), with bun at 635. It flips one pair: oam cache-off (807) was
slower than cold + produce (683), where the interleaved series has 578 vs 946. On
`--help` it does not show oam warm ahead of node warm (998 vs 931). Its per-series probe
medians were 128-844 ms, so a single series is more exposed to load than the
interleaved one.

#### Split: pre-eval vs evaluation vs teardown (9 interleaved rounds; ms, medians)

These runs used `cli-timed.js`, a second copy of cli.js (cli-patched.js for oam; the same
probe line for all three runtimes). Its only change is one line after the shebang:
`process.stderr.write("T1 "+Date.now())` plus a `process.on("exit")` that writes T2. ESM
imports are hoisted, so T1 marks the first statement of the module body. That is after the
process booted, read, parsed/compiled (or consumed a cache entry) and linked the graph.
- **pre** = spawn to T1
- **eval** = T1 to the `exit` event
- **post** = `exit` event to the parent seeing the exit

| config | total | pre | eval | post | total min-max |
|---|---:|---:|---:|---:|---|
| `--version` (a) oam cache off | 556 | 521 | 13 | 12 | 482-2180 |
| `--version` (b) oam cold + produce | 674 | 574 | 98 | 11 | 557-984 |
| `--version` (c) oam warm | **349** | **320** (min 179) | 13 | 15 | 201-1885 |
| `--version` (d) node cold | 1057 | 1006 | 14 | 24 | 674-1727 |
| `--version` (e) node warm | 702 | 675 | 17 | 23 | 420-1890 |
| `--version` (f) bun | 1105 | 1006 | 14 | 61 | 538-2969 |
| `--help` (a) oam cache off | 1305 | 748 | 361 | 40 | 800-4064 |
| `--help` (b) oam cold + produce | 1024 | 666 | 369 | 44 | 800-2669 |
| `--help` (c) oam warm | **550** | **224** (min 168) | 313 | 33 | 425-934 |
| `--help` (d) node cold | 1485 | 945 | 419 | 19 | 1099-2060 |
| `--help` (e) node warm | 1052 | 553 | 470 | 21 | 726-1323 |
| `--help` (f) bun | 1136 | 654 | 303 | 35 | 735-2115 |

What the split shows:
- **`--version` is 95% pre-eval on every runtime.** The bytecode cache is what moves it:
  oam's pre-eval drops from ~520 ms to ~180-320 ms.
- **`--help` adds ~300-470 ms of real evaluation** (building commander's command tree and
  rendering help) on all three runtimes. oam's warm cache trims only ~50 ms of that, which
  is consistent with the blob already holding most of the functions evaluation compiles.
  The rest is JS execution, not compile.
- **(b) cold + produce shows +85 ms in eval** (98 vs 13). That is
  `create_code_cache()` plus the 4.2 MB atomic write. It runs after the graph evaluates
  and before `exit` fires, so the first run pays it inside the process.
- **Warm pre-eval minimum is ~170-180 ms.** About 40-50 ms of that is oam boot (the
  `empty.mjs` minimum). The rest is reading 13.7 MB of source, hashing it to a SHA-256
  cache key, reading the 4.2 MB blob, V8 deserialize, and link. These sub-costs were not
  separated further.

#### CPU profile via CDP (`oam run --inspect-brk`, with a small node WebSocket driver)

**Feasible, but it distorts the cached case.** The driver does the following, in order:
1. `Profiler.start` before `Runtime.runIfWaitingForDebugger`. oam waits for attach before
   loading the graph, so the profile covers load and compile.
2. Stops the profile at the "Break on start" pause. That pause lands just before the
   entry evaluates.

Results:
- **Cache-off profile (1,096 samples, 1.46 s with the profiler on):** 86% is `(program)`
  (1,257 ms, native: read/parse/compile/link), 3% GC (50 ms) and ~25 ms of JS (oam's
  `node_compat.js` builtin getters). cli.js JS itself is 1.4 ms before its first statement.
- **Repeated inspector runs without the profiler (5 per cell):**
  - run-to-entry-pause: 566-1565 ms (cache off) and 602-1249 ms (warm)
  - entry-to-exit: 21-50 ms (`--version`) and 268-889 ms (`--help`)
- **Under an attached inspector, the warm cache gives no measurable benefit.** That
  contrasts with ~250-300 ms without one. The cause was not checked: either V8 does not
  consume or does not benefit from the cache with `Debugger.enable`, or the inspector's
  per-script work on a 13.7 MB source dominates. So use the T1/T2 probe split above, not
  `--inspect`, to measure cache effects.
- A profile of evaluation up to exit was not collected: oam does not keep the inspector
  alive at exit, and an injected `process.on('exit', () => { debugger })` did not pause.
  The T1/T2 split covers evaluation instead.

oam has no `--cpu-prof` and no V8-flag passthrough (`oam_cli/src/main.rs:651-662`
hardcodes three flags), and rusty_v8 150 has no CpuProfiler start/stop binding, so CDP
`Profiler.*` over `--inspect-brk` is the only profiling route today (code read).

#### Not done

The TUI-prompt timing (the 3.0-3.6 s figure) was not measured; it needs a ConPTY driver.
That is where the remaining question lives. With pre-eval at ~0.2-0.3 s warm, the next
measurement for Gap 4 should time the TUI path with the same T1 probe plus a mark at the
first prompt render. Then attribute the remainder to app init vs waiting (git, MCP
servers, hooks, network).

Raw data and the bench/CDP scripts were kept outside the repo with the session's
scratch files; they are not committed.

### 4b. Node's compile-cache API on oam's existing cache, plus a size cap

Not a speed item (4a: the cache already exists and works). What was missing is node's
API surface and cache hygiene.

**Status (see "Where the work is").** Branch `feat/compile-cache` implements the API
part: `enableCompileCache`, `getCompileCacheDir`, `flushCompileCache`,
`constants.compileCacheStatus`, `NODE_COMPILE_CACHE` and `NODE_DISABLE_COMPILE_CACHE`,
with node v22.22.2's return shapes, statuses and messages, conformance cases 438-439, e2e
tests, a CHANGELOG entry under [0.19.1] and a divergence section ("The compile cache is
on by default, and keeps oam's own layout"). Where it settled differently from the
proposal below: a directory gets a per-build `oam-v<version>-<arch>-<V8 tag>`
subdirectory (not `<dir>/oam/`), `enableCompileCache()` with no directory answers
ENABLED with oam's default root and does not move the cache, and `process.exit()` /
`flushCompileCache()` also write CommonJS modules whose body is still running. The size
cap and the touch on hit are NOT on the branch (its `code_cache.rs` still says blobs are
never rewritten on a hit), and neither are the smaller gaps listed last. The proposal is
kept below as the record of what was asked.

- **API parity.** On `main`, `node:module` exports only `createRequire`, `builtinModules`,
  `isBuiltin`, `syncBuiltinESMExports` and `Module` (`node_compat.js:16511-16526`).
  `module.constants` (`compileCacheStatus`), `module.enableCompileCache()`,
  `module.getCompileCacheDir()` and `module.flushCompileCache()` are listed as gaps in
  `surface-gaps.json` (e.g. win32 :302-307). `NODE_COMPILE_CACHE` and
  `NODE_DISABLE_COMPILE_CACHE` are not honored (the only hit is `bench.rs` removing
  `NODE_COMPILE_CACHE` from node's environment). Proposed shape, layered on the
  existing store and keeping its content-addressed keys:
  - `module.constants.compileCacheStatus` = frozen `{FAILED, ENABLED, ALREADY_ENABLED,
    DISABLED}` with node's numeric values (recalled as 0-3; pin against node v22.22.2's
    `lib/internal/modules/helpers.js` and `src/compile_cache.h` before the spec).
  - `enableCompileCache(dir)` returns `{status, message?, directory?}`: DISABLED with
    node's message when `NODE_DISABLE_COMPILE_CACHE` is set or `OAM_CODE_CACHE=0`;
    ALREADY_ENABLED with the directory when already enabled explicitly (env or a prior
    call); otherwise ENABLED, even though oam's cache is on by default; FAILED with a
    message when the directory cannot be created. A directory redirects the store to
    `<dir>/oam/` for modules compiled afterwards, so oam's blobs never mix with node's
    version-hash subdirectories and the sweep never deletes node's files.
  - `getCompileCacheDir()` returns the active dir, or `undefined` when disabled.
  - `flushCompileCache()` writes pending blobs synchronously.
  - `NODE_COMPILE_CACHE=dir` is read once at startup and redirects to `<dir>/oam`.
    Precedence: `NODE_DISABLE_COMPILE_CACHE` > `OAM_CODE_CACHE=0` >
    `NODE_COMPILE_CACHE` > `OAM_CACHE_DIR` > platform dir.
  - The cache root is a `OnceLock` today (`code_cache.rs:167-170`), so runtime
    redirection needs a small refactor.
- **Size cap and LRU.** Add a cap to the daily sweep (e.g. `OAM_CODE_CACHE_MAX_MB`,
  default around 1 GiB): oldest mtime first, down to ~80% of the cap. Add a throttled
  touch on hit (at most one mtime update per blob per day) so a blob in daily use is not
  evicted at 30 days.
- **Smaller gaps found on the way.** A CJS module whose body throws is never cached,
  while ESM caches on the throw arm (`cjs.rs:759-764` vs `modules.rs:823-826`).
  Related fake-success stubs: `vm.Script.createCachedData` returns an empty
  `Uint8Array` (`node_compat.js:32666-32668`), `v8.cachedDataVersionTag` returns 0
  (:32894). These share lines with gap 1's v8 block and with `feat/require-esm`'s
  `modules.rs`/`cjs.rs` edits; sequence them.

Accept: the four `module` names and two env vars behave as node's on the cases above
(node-differential where the output is comparable), removed from all three
surface-gaps sections; the cap and the touch covered by tests; CHANGELOG entry.
Public-API addition, so two-human review.

### 4c. App startup snapshot (stretch)

Snapshot the V8 heap after the app's modules have evaluated, so the next start
deserializes it and skips compile AND top-level evaluation. Node: `--build-snapshot` /
`--snapshot-blob` / `v8.startupSnapshot`. The design is in
`docs/design/app-startup-snapshot.md` (draft for review). Its main corrections to the
first draft's sketch:
- "oam already compiles a snapshot of its own internals, so the machinery exists" is
  only half true: that snapshot is built in `build.rs` with no external references, and
  every native is installed after deserialize. An app snapshot captures Rust-backed
  functions, so it needs a complete external-reference table generated from the same
  list that installs the natives.
- After 4a, the most 4c can win on cli.js is the ~150-250 ms of warm pre-eval plus
  whatever app init a snapshot can capture -- and for an unmodified single-bundle app
  whose top level IS the program, likely ~0 without app cooperation
  (`v8.startupSnapshot.setDeserializeMainFunction`). So 4c cannot beat the native
  1.4 s for cli.js; it is for apps that opt in.
- A PR 0 that makes the `v8.startupSnapshot` stubs honest (and adds the missing
  `setDeserializeMainFunction`) is worth shipping regardless.

Accept (unchanged): an app reaching first output faster from a snapshot than from a
warm compile cache, with the design's rules tested.

## Order and sizing

| # | Item | Effort (focused days) | Status 2026-10-15 |
|---|---|---|---|
| 1 | Gap 1 heap snapshot: Stage B (Stage A only as a fallback) | 2.5-3 | PR #410 open; missing space stats and the divergence policy text |
| 2 | Gap 4a measurement | < 0.5 | done (above); TUI-prompt timing still open |
| 3 | Gap 2 step 1 (Windows std-handle unshare) | 1-1.5 | test + fix branches committed; PRs to be opened |
| 4 | Gap 3 native fs.watch (PR 0 stopgap: probably drop) | 8-11 | PR #409 open; macOS kqueue file path, surface-gaps counts, measure-first, gate-3 ruling open |
| 5 | Gap 1 extras: +1 cross-isolate Worker snapshot, +1.5-2 other 8 v8 names | 2.5-3 | not started |
| 6 | Gap 2 step 2 full inheritance parity | +1.5-2 | test + fix branches committed (stacked on step 1); PRs to be opened |
| 7 | Gap 4b node compile-cache API + size cap/LRU | 2-3 (inferred) | API implemented on `feat/compile-cache`, no PR yet; size cap/LRU not started |
| 8 | Gap 4c startup snapshot, after design review | see its design doc | design draft |
| - | Gap 2 follow-up: HANDLE_LIST in spawn_child | 3-5, separate | not started |
| - | Re-measure the `--help` linger in an idle ConPTY | < 0.5 | unowned |

Before more of this becomes PRs (1 and 4 already are): one maintainer ruling on
spec-first vs test-with-fix, which PRs #409 and #410 also need. The in-flight branches
conflict in
`CHANGELOG.md`, the node-divergences gap table (:3858-3866), `surface-gaps.json`
(all three host sections and the counts), the v8 factory in `node_compat.js`, and the
`node_ops.rs` op table; whichever merges second recomputes the surface-gaps counts by
hand for linux and darwin.

Finish line: unpatched cli.js 2.1.112 runs `--version`, `--help`, `-p` and the TUI on
oam on all three OSes, with idle CPU at node parity and no detached-child pipe hang.
(Warm start is already faster than node's warm `NODE_COMPILE_CACHE` on `--version`, and on
`--help` in the interleaved series only; see 4a.) Consider adding cli.js `--version` /
`--help` / TUI-render as a pinned real-app smoke in the conformance corpus (`-p` needs
network). Nobody has verified the finish line on all three OSes yet.

Ceiling, stated plainly: all of this improves oam and the frozen 2.1.112. It buys
current Claude Code users nothing.
