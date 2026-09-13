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

- **On Windows, going raw right after a prompt could overwrite the answer line.**
  `setRawMode` flipped the console mode first and cancelled the stdin read in
  flight afterwards, so the synthetic Enter that cancels the read was handled
  under the NEW mode -- and a raw read returns that Enter as one silent byte, a
  bare carriage return with nothing echoed, so no newline scrolled the buffer the
  way libuv's last-row cursor adjustment assumes. With the prompt on the screen
  buffer's last row the cursor came back one row too high and the next output
  landed on top of the answer (reproduced in a real conhost: "name? bob" became
  "MARK? bob"). The switch now runs in libuv's order -- hold the reader, cancel
  while the read's own mode is still in force, flip, release -- and what the Enter
  writes to the screen is derived from that pre-flip mode: a line read writes it
  whether or not it echoes, CRLF with `ENABLE_PROCESSED_INPUT` and a bare CR
  without, while a raw read writes nothing (`readDataCooked.cpp`, measured on
  conhost 10.0.26100.1), so the cursor steps up a row only when a newline actually
  scrolled it. The switch is also serialised process-wide now, so a second
  `setRawMode` -- a Worker's, or the exit hook's -- cannot read the console mode
  or the saved slot mid-switch and save a raw mode as the "original". A
  `setRawMode(false)` whose `SetConsoleMode` fails also keeps the saved original
  mode now, where it used to be taken out of the slot and lost with the error,
  leaving the exit hook nothing to put back. (#125)
- **A raw program killed by SIGINT or SIGTERM left the terminal raw.** oam
  installed a native handler for a signal only when a JS listener asked for one,
  so a program that had called `setRawMode(true)` and was then killed without one
  died at `SIG_DFL` with the exit hook never running -- and a shell left raw does
  not recover by itself. `setRawMode(true)` now arms a process-wide default action
  for SIGINT and SIGTERM (`signal::serve_default_action`), which restores the
  terminal, then restores `SIG_DFL` and re-raises, so the process still dies by
  that signal and the parent sees it; Node does the same from a handler it
  installs at startup whether or not JS listens (`SignalExit` -> `ResetStdio` in
  `src/node.cc` -- read from Node's source). The restore follows
  `uv_tty_reset_mode` rather than the ordinary switch -- `TCSANOW` where the
  switch drains with `TCSADRAIN`, SIGTTOU blocked, retried on `EINTR`, and giving
  up rather than waiting behind a switch on another thread -- so a stalled
  terminal cannot hold a dying process. A stop is not a death: a SIGTSTP with no
  listener leaves raw mode intact, as Node does. **This is a behaviour change** on
  Unix; Windows is untouched. (#125)
- **A signal could be swallowed, or kill the process under another isolate's
  listener.** Each signal handle reproduced the OS default as soon as its own
  listeners were gone, which a second isolate made wrong in both directions: a
  dormant handle in one isolate killed the process while another isolate was
  still listening, and a handle that died with its run -- `oam test` builds a
  runtime per file, and a Worker or `oam.fork` isolate has its own -- left
  tokio's process-global handler installed with nobody receiving, so every later
  delivery was caught and discarded (a SIGINT during the second file of
  `oam test` hung the runner). Whether anyone is listening is now decided for the
  whole process, from a count of watched handles across every isolate, and one
  never-dropped task per signal serves the default. (#125)
- **`scripts/bump-taps.sh` crashed inside its own EXIT trap on an early abort.**
  `cleanup()` guarded the two tap directories but dereferenced `$BREW_FILE` and
  `$SCOOP_FILE` bare, and both are assigned long after the trap is armed -- so
  under `set -u` every failure in that window (no published `SHA256SUMS`, the
  downgrade guard, a missing asset hash) died with `BREW_FILE: unbound variable`
  inside the trap, appending a bash error about the script's own bookkeeping to
  the diagnosis the operator actually needs -- and aborting the rest of `cleanup`
  behind it. Both filenames now carry the same `${:-}` guard the directories had,
  and a test case asserts the crash is absent on that early-abort path: the
  pre-existing downgrade case could not catch it, because `fail` prints its
  message BEFORE the trap runs, so grepping for that message passed either way.
  (#127)
- **The sidecar release gate's "verified against node" rows were oam against oam.**
  Every @yawlabs sidecar's `bin` is a runtime launcher that prefers oam, so the
  control arm's `node <bin>` re-spawned oam -- measured with a process-tree walk --
  and fetch and ctxlint passed a comparison that compared oam with itself. The control
  arm now sets the launcher's own `*_RUNTIME` switch to `node`, read from its source
  rather than listed, and a launcher that names no switch is refused instead of
  trusted.
- **Every sidecar in the release gate now answers a real tool call; six were
  boot-only.** None needs a credential or the internet: tailscale's network-free
  `tailscale_tool_groups`, Lemon Squeezy's webhook sink pointed at the loopback
  server, redis against a Redis wire-protocol fake with fixed replies, postgres
  against the local server with a SELECT that crosses bytea, and puppeteer and
  playwright driving the Chromium-family browser already installed, headless, in a
  profile the harness owns. A dependency that is genuinely missing DEMOTES the row
  with the reason and makes the run incomplete (exit 3) -- before, a loopback bind
  that failed quietly turned fetch into "boot only" and exited 0. Inherited
  credentials and configuration are scrubbed from each sidecar's environment, so
  "needs no credential" holds on a box that has one.
- **The release gate now fails a sidecar that leaves a process behind.** Each probe
  shuts the sidecar down the way a host does (stdin closed, then killed) and checks
  its direct children against the node control. On Windows, node's libuv places
  children in a kill-on-close job object and oam does not, so on oam 0.15.1 both
  browser sidecars leave their browser running where node leaves none -- and the gate
  now says so rather than passing them.
- **puppeteer no longer SKIPs the gate on an interrupted browser download.** Its
  postinstall fetches a Chrome the gate never used, and a half-extracted copy in the
  user cache failed the whole batch install. `PUPPETEER_SKIP_DOWNLOAD=1` is set for
  the install, and a failed install now reports its error or its timeout rather than
  the first deprecation warning npm printed.
- **On Windows, a child outlived the oam process that spawned it when that process
  was killed.** node's libuv puts every non-detached child in a process-global job
  object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, and the kernel closes the job's
  only handle however the parent ends -- a clean exit, a crash, `TerminateProcess` --
  which takes the children down with it. oam had no job, so a sidecar killed by its
  host left its browser running, and a script that spawned
  `node -e "setInterval(()=>{},1e9)"` with stdio ignored still had it running 1.5s
  after the script was killed, where under node it is gone. oam now keeps the same
  job, flag for flag (`KILL_ON_JOB_CLOSE`, `BREAKAWAY_OK`, `SILENT_BREAKAWAY_OK`,
  `DIE_ON_UNHANDLED_EXCEPTION` -- so a grandchild a child starts on its own is not a
  member, exactly as under node), on every path behind `child_process` and
  `cluster`: `spawn`, the extra-fd `CreateProcessW` spawn, `execFile` and the shell
  `exec` starts, `fork`, `cluster.fork`, and `spawnSync`, whose child now dies too if
  oam is killed while blocked on it. The extra-fd path creates the child suspended
  and resumes it only once it is in the job; tokio's and std's `Command` cannot
  resume a suspended child, so those paths join it right after the spawn, which is
  where libuv joins every child. `detached: true` children stay out of the job --
  and now also get libuv's `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`, where oam
  used to ignore the option on Windows -- as does the `oam_ts` checker daemon. A job
  that cannot be created or joined (oam itself inside a job that forbids nesting)
  degrades to the old behaviour rather than failing the spawn. POSIX is untouched: a
  child there outlives its parent under node as well. A new e2e test kills a real
  `oam run` with `TerminateProcess` and checks a child from each of those seven paths
  is gone and a `detached` one is not; it fails on 0.15.1, where all eight survive.

### Changed

- **The sidecar release gate reports what it tested.** Each row carries the resolved
  sidecar version, the summary states how many of the advertised tools were actually
  called, `--json=<path>` writes a machine-readable report (release-local.sh keeps it
  beside the conformance stamps, outside the published assets), and progress text is
  only rewritten in place on a terminal, so a captured log no longer runs lines
  together.
- **A failed `setRawMode` emits Node's error shape.** oam built an error with
  `code` `'ERR_SYSTEM_ERROR'` and `syscall` `'uv_tty_set_mode'`; Node v22.22.2's
  `lib/tty.js` emits `new ErrnoException(err, 'setRawMode')`, whose `code` is
  `util.getSystemErrorName(err)`, message is `setRawMode <code>` and `syscall` is
  `'setRawMode'`, with `"Unknown system error <n>"` for an errno the table does
  not map. **This is a behaviour change**: a program branching on `err.code`
  never matched before. Checked against Node's source only -- no test reaches this
  branch: it needs a console that refuses `SetConsoleMode`, and the one harness
  that has such a console (#109) is `#[ignore]`d. (#125)
- **The Windows console mode oam leaves behind is documented** as divergence 33 in
  `docs/node-divergences.md`. libuv's `uv_tty_set_mode` writes a fixed input mode
  in each direction and only restores the startup mode at a normal exit, while oam
  clears `ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT` from the
  mode it finds, adds `ENABLE_VIRTUAL_TERMINAL_INPUT`, and puts back the mode it
  saved on `setRawMode(false)`. Going by the source the program reads the same
  bytes either way; what differs is the console mode between a raw round trip and
  exit, which shows to anything that queries or reads the console in that window
  -- the program itself, or a child that inherits the console without setting its
  own mode. No behaviour changed here; it is written down. Read out of both
  implementations, not measured side by side. (#125)

## [0.15.0] - 2026-09-10

A release about the ways into oam and the boundaries around it: two security fixes
reachable from an ordinary run -- the permission matcher and the MCP HTTP transport --
three correctness fixes in `fetch`, `fs` and the heap cap, the first npm packaging,
and a set of release gates that stop a stale or broken artifact reaching a public
channel.

### Added

- **An `oamjs` npm launcher, so `npx` will be able to run oam.** The only install paths
  were the two piped installers -- `curl https://oamjs.org/install.sh | sh` on Linux and
  macOS, `irm https://oamjs.org/install.ps1 | iex` on Windows -- plus two package-manager
  taps nobody was updating (see below), and a piped installer asks someone to pipe a
  script from an unfamiliar domain into their shell before they have run a single line of
  oam. The launcher's five per-platform binary packages are `optionalDependencies`, so npm
  downloads one platform binary rather than five, and there is **no postinstall
  script** -- a postinstall that fetches a binary contradicts what `oam trust` and
  OAM-PKG0007 exist to argue, and breaks behind proxies, in offline CI and in any
  sandbox with no network at install time. The launcher spawns with `stdio: 'inherit'`
  and forwards signals rather than using `spawnSync`, because an MCP host stops a server
  by killing the PID it spawned -- which is the launcher, not oam -- and the default
  SIGTERM disposition would have left oam alive holding the host's pipes. Linux arm64
  has no binary -- the V8 startup snapshot must be generated by a process of the target
  architecture, so it cannot be cross-compiled -- and the launcher refuses it by name,
  pointing at an x86_64 host or a source build, instead of leaving npm's
  skipped-optional-dependency error to blame the user's install. All six manifests are
  generated from the workspace version by `npm/sync-packages.mjs`, whose `--check` exits
  non-zero on drift. Nothing is published to the registry yet -- this release ships the
  packages and the sync gate, not the npm entry point. (#117)
- **oam's own modules now type-check.** `import { McpServer } from "oam:mcp"` runs --
  the loader maps the specifier to a snapshot module -- but tsgo resolved it like any
  other bare specifier and answered TS2307, so the runtime whose pitch is typed
  TypeScript rejected its own flagship module from its own checker. Same for `oam:test`,
  `oam:ai` and `oam:permissions`, and TS2304 for the `oam` global.
  `crates/oam_ts/types/oam.d.ts` declares all four modules and the global, every name
  read off the JS that publishes it, and `oam check` injects it: as one more root file
  for a bare-file check, and through a generated wrapper config that `extends` the
  user's for a project check, since tsgo refuses a file argument next to `-p`. Every
  failure path checks without the declarations rather than refusing to run. `xtask
  conformance` now diffs the declared names against the module objects the runtime
  publishes and fails both ways -- an export with no declaration, and a declaration
  nothing exports -- so hand-written declarations cannot rot inside a release. (#119)
- **A raw control byte in a tracked file is now a gate failure.** A NUL inside a string
  literal is semantically valid, so fmt, clippy, the build and the tests all pass with
  it present, and `git diff` reports the file as "Binary file ... matches" with no
  hunks, which makes it unreviewable rather than merely easy to miss; five literal NULs
  reached main in a sibling repo exactly that way. `scripts/check-control-bytes.sh` runs
  as step 1 of 14 in `ci-local.sh`, first because it is the cheapest step (~1s over the
  whole tracked tree) and because a finding invalidates everything after it. The scanner
  itself had to be fixed before it could be wired in: it was one `git grep -I`, and `-I`
  skips files git considers binary -- which is exactly what a file containing a NUL is
  -- so it reported clean on a planted NUL. It is now a node pass over `git ls-files`
  (or the index under `--staged`, since a pre-commit gate must read what is about to be
  committed) with an explicit binary-extension denylist; it names file, line, byte and
  offset without echoing the matched line, and a missing node exits 2 ("could not run")
  rather than reporting clean. Later rounds closed four more false-clean paths: unmerged
  paths in a merge resolution, tracked files absent from disk, `.snap` files wrongly
  denylisted as binary, and an escape hatch that was a bare substring, so any file
  merely mentioning `control-byte-ok` became permanently unscannable -- it is now a
  declaration on its own line.

### Fixed

- **A granted permission also granted things it did not name.** One
  `target.starts_with(item)` served every category, so `--allow-net=api.github.com` also
  granted `api.github.com.attacker.net`, a name someone else can register;
  `--allow-net=127.0.0.1:5432` also granted port 54321; `--allow-env=API` also granted
  `API_SECRET`; and `--allow-fs-read=/box/allowed` also granted the sibling directory
  `/box/allowed-evil` and `/box/allowed/../../secret.txt`. Each category now has its own
  rule: fs is a path prefix anchored at a separator over a lexically resolved target
  (lexical rather than canonicalised, because the write gate runs on files that do not
  exist yet); net is exact on host:port, where an entry with no port grants any port on
  that host and a bracketed IPv6 literal is split on its bracket rather than at the last
  colon; env is exact. `worker`, `child` and `ffi` route to the path matcher, each
  gating a path-shaped resource. **This is a behaviour change**: a grant that used to
  reach a neighbouring host, port, variable or directory no longer does. Also fixed:
  `query_state` had six arms for seven fields, so `permissions.query({name:'worker'})`
  reported denied on a default all-granted run. Symlink escape is explicitly *not*
  closed -- resolution is lexical, so a symlink inside an allowed subtree still leads
  out of it, and the module docs now say so.
- **A web page you merely visited could invoke your MCP server's tools.** The HTTP
  transport had no `Origin` check, and `detectTransport()` selects HTTP whenever stdin
  is a TTY, so a plain interactive `oam run server.js` listens on 127.0.0.1 -- which is
  not a boundary against a browser, since a CORS-simple POST needs no preflight and
  lands even though the page cannot read the reply. Blind is not harmless when the tools
  have side effects. Requests whose `Origin` falls outside `serve()`'s new
  `allowedOrigins` are refused with 403 before any routing; the list is empty by
  default, and a request with no `Origin` header -- every non-browser client -- is
  allowed. The same gate closes DNS rebinding. Two further holes in the same handler:
  the well-formed body `null` parses to `null`, and `#handleSingle` read `.id` off it
  before validating anything, with the throw escaping an un-awaited `createServer`
  callback, so it was not a failed request but OAM-RT0004 and a dead runtime -- and on
  the SSE path the 202 had already been written, so the client saw success against a
  server that no longer existed; it now answers -32600. And SSE session ids were an
  incrementing integer parsed with `parseInt`, so they were guessable and
  `?sessionId=1abc` resolved to session 1; they are now `crypto.randomUUID()` strings
  compared verbatim.
- **`fetch` handed JavaScript compressed bytes instead of the body.** It advertised no
  `accept-encoding` and decoded nothing, so a server that compressed anyway returned raw
  DEFLATE with `content-encoding: gzip` still on the response -- not an error, just a
  body that is not the body, on the path an MCP sidecar or an HTTP service takes to
  reach an upstream. oam now advertises exactly what Node v22.22.2 advertises, measured
  rather than assumed: gzip and deflate, not brotli, since asking for `br` would trade
  one divergence for another. Both clients get it, including the pinned client used for
  connect-hook fetches. The decoded response no longer carries `content-encoding` or
  `content-length`, where Node keeps both: reqwest strips them as it decodes and the
  pre-strip values are not recoverable above it. Recorded as divergence 32. (#114)
- **`fs` treated a `file:` URL as a filename.** Every entry point coerced its path
  argument with `String(path)`, which yields the href, so
  `writeFileSync(new URL('./data.txt', import.meta.url), 'hello')` asked the OS to open
  a file literally named `file:///C:/...` and failed with ENOENT -- and
  `new URL('./x', import.meta.url)` is the mandated ESM replacement for `__dirname`, so
  this is how modern packages address files next to themselves. One `toPath` helper now
  owns the coercion for string, Buffer and `file:` URL across all 88 call sites,
  covering sync, promises and callback forms and both path positions of `rename` and
  `copyFile`; a non-`file:` URL is refused with `ERR_INVALID_URL_SCHEME` rather than
  stringified. Two follow-on shapes came with it. The promises forms now *reject* on a
  bad path instead of throwing synchronously, matching node -- a synchronous throw
  escapes an already-attached `.catch()` and escapes `Promise.all`, so one bad path took
  down a whole batch a supervisor expected to settle -- while the sync and callback
  forms keep throwing, as node does. And `fs/promises.glob` stays async-iterable: it is
  the one member that returns no promise, and wrapping it broke `for await` loudly but
  broke `await Array.fromAsync(fsp.glob(p))` silently, which read `length === undefined`
  and resolved to `[]`, so a build or deploy step that globs for files found zero and
  carried on. (#114)
- **`brew install oam` served 0.8.1 while the project shipped 0.14.0.** Nothing owned
  the package-manager taps: the Homebrew formula was hand-written once at v0.8.0 and
  never touched again, and the Scoop manifest was pinned to the same version -- eleven
  releases and five weeks stale, and `oam self-update` cannot rescue a brew-managed
  prefix, because it is not the installer's per-user directory. `scripts/bump-taps.sh`
  now rewrites and pushes both on every release, and can repair a drifted tap without
  cutting one. The hashes come from the release's own published SHA256SUMS -- the same
  authority `install.sh` and `install.ps1` verify against -- re-read at that step rather
  than carried from earlier in the run, and the formula rewrite is asset-keyed rather
  than positional, since pairing by position is how a formula ends up serving the mac
  hash for the linux binary. It fails closed on a missing or malformed hash, on a tag
  older than the newest release (unless `OAM_ALLOW_DOWNGRADE=1`), on a missing upstream
  branch, and on anything that would move another session's uncommitted work in these
  shared checkouts; an EXIT trap reverts every un-published rewrite on any abort, so a
  failed run cannot leave the next one blaming the operator for this script's leftovers.
  Verification reads what origin serves and checks the hashes rather than just the
  version string, with a per-run cache key, because a manifest naming the right version
  with a wrong hash fails on every user's machine and raw.githubusercontent caches for
  about five minutes. (#115)
- **`oam run --inspect app.ts` did not run.** clap's `num_args = 0..=1` without
  `require_equals` let the flag take the next argument as its value, so the FILE was
  consumed and the run died with "the following required arguments were not provided:
  <FILE>" -- which is the first line of the inspector documentation. Bare, `=host:port`
  and node's own shape all work now. (#118)
- **Two printed URLs did not resolve, and both had been broken for months.** Every
  `--inspect` / `--inspect-brk` run printed "For help, see:
  https://oamjs.org/docs/inspector", a page that did not exist, so the line a user reads
  when the debugger is misbehaving sent them to a 404; that page has since been written
  and published from the oamjs.org checkout, out of this repo, and the printed line
  itself is unchanged. And every ODIF diagnostic carried
  `"docs":"https://oam.sh/e/<code>"`; oam.sh stopped being the project's domain and now
  redirects elsewhere, so under `--json` oam handed every reader of every diagnostic to
  somebody else's site. Diagnostics now carry a `docs_url` pointing at
  https://oamjs.org/docs/errors anchored at the code, with pass-through TypeScript
  diagnostics (`OAM-TS<n>`, carrying tsgo's own number, an unbounded family) landing on
  the family section rather than on an anchor no page can honour. A gate --
  `docs/published-urls.txt` plus `xtask/tests/published_urls.rs` -- now requires every
  project-host URL printed by `crates/*/src`, `js/` outside the vendored node-streams,
  and `xtask/src` to appear in a checked-in list with a note saying what prints it, in
  both directions so the list cannot rot; retired domains are refused outright and
  cannot be allowlisted. It does not fetch anything, so it passes offline. (#118)
- **`oam trust --help` said script execution "is not yet supported".** It is:
  `run_lifecycle_scripts` executes a trusted package's preinstall, install and
  postinstall through the platform shell. The help therefore described an
  arbitrary-code-execution grant as though it merely suppressed the OAM-PKG0007 warning.
  Both doc comments now say what trusting actually does, and name `OAM_IGNORE_SCRIPTS`
  as the global off. (#118)
- **`oam explain` invented TypeScript codes.** The `OAM-TS` fallback matched zero-padded
  codes too, so `OAM-TS0005` -- oam's own concurrent-check warning -- was answered as
  "TypeScript diagnostic TS0005 ... search the TypeScript docs", a code that does not
  exist. Zero-padded codes now fall through to the family text, which at least admits it
  does not know. (#118)
- **`runToolLoop`'s own documented example was a silent no-op.** The doc comment showed
  `chat: anthropic(key).chat`, but the presets are async generators and awaiting a
  generator returns the generator, so `.content` and `.choices` were both undefined, no
  tool call was ever detected, and the caller got `{ text: '', iterations: 1 }` --
  and because generators are lazy, no HTTP request was made at all. The example is a
  real non-streaming chat now, and an async-iterable response throws a TypeError naming
  the fix rather than failing quietly. `runToolLoop` was also the one `oam:ai` export
  with no behavioural test and now has one. Separately, README.md and docs/why-oam.md
  claimed 429/431 conformance on windows-aarch64 with two deliberate failures, two lines
  below README's own statement that the receipts are never hand-edited; the generated
  scorecard says 439/442 with three. The figures are corrected to the receipt, the macOS
  and Linux numbers are dropped rather than updated (no scorecard for those hosts is
  committed here, so they were presented as though they had one), and a test now
  compares the documented figures against the JSON.
- **`OAM_NPM_ALLOW_UNEXECUTABLE=1` did nothing.** `npm/stage.mjs`'s Windows refusal told
  the operator to set it to override, and the condition never read the variable, so the
  only way to pack a POSIX package locally was to edit the script. Found because a new
  test tried to use it. (#122)
- **A conformance case flipped its verdict about a quarter of the time.**
  `97-stdin-ref-and-destroy` measured elapsed time from the `spawn()` call, so process
  creation and runtime boot -- 550-710ms for a debug-build child on the measuring box --
  sat inside every measurement and pushed children that HAD released stdin past the
  1200ms line. The child now prints a ready marker as its first statement and the clock
  starts there, so only the question the case exists to ask is inside the measurement.
  Six consecutive runs now produce identical output on both runtimes. (#120)

### Changed

- **The heap cap is derived from the container memory limit.** With `OAM_MAX_HEAP_MB`
  unset the cap was a hardcoded 4 GiB and nothing read a cgroup, so a 512Mi pod ran a
  runtime that believed it had 4 GiB: it never reached V8's near-heap-limit callback and
  the deterministic banner oam already owns, it was killed by the kernel -- exit 137, no
  banner, no crash file, no ODIF, and nothing in the logs tying the death to memory. oam
  now reads cgroup v2's `memory.max` and v1's `memory/memory.limit_in_bytes` and derives
  the cap as 75% of the limit, because the heap is not the process: thread stacks,
  ArrayBuffer backing stores and V8's own off-heap bookkeeping sit outside it, and
  handing over the whole limit just moves the kernel kill to a different threshold. A
  derived cap below 128 MB or at or above the 4 GiB default is declined and the built-in
  default stands. **This is a behaviour change** for a containerised deployment that was
  relying on the flat 4 GiB; `OAM_MAX_HEAP_MB` still wins, and the OOM banner names the
  new provenance and the override. Node does not do this -- its
  `--max-old-space-size` default ignores cgroups -- so this is a deliberate divergence
  rather than parity work. The platform split lives in the limit reader, not around the
  policy, so the fraction and floor rules stay compiled and unit-tested on Windows and
  macOS too. (#114)
- **The MCP sidecar release gate now invokes a real tool call.** `tools/list` is answered
  by a sidecar's registration table, which stays intact while everything behind it is
  broken -- so the gate standing between an oam release and every Yaw MCP user (Yaw MCP
  defaults node/npx sidecars to oam on any machine that has it) passed a sidecar whose
  every tool threw on invocation. Each sidecar that has a tool needing no credential, no
  network and no external service now gets it called and the result asserted: memory
  `read_graph`, fetch `http_get` against a loopback server the harness starts, ctxlint
  `ctxlint_token_report` over a fixture it writes. The other six advertise nothing that
  qualifies and are reported boot-only by name, each with its own reason, counted in a
  separate column rather than collapsed into coverage the gate does not have. Every
  invocation also runs on node and the two are adjudicated: a failure reproduced on node
  is upstream and never reddens a release, a failure only on oam holds it, and "the
  control could not run" now yields a fail instead of being read as "the control agrees"
  -- which had been excusing a real oam regression whenever the control host could not
  boot the sidecar. Boot failures consult the control too, and an error reply to
  `initialize` is diagnosed immediately instead of waiting out the 90s boot timeout and
  then reporting the wrong phase. (#116)
- **A release whose CHANGELOG `[Unreleased]` section is empty is refused before the tag
  exists.** RELIABILITY.md's semver policy already promised that every release ships a
  public behaviour-change log; nothing enforced it, and the omission is invisible
  afterwards, because the tag makes the CHANGELOG look complete for every version that
  does have entries. The check also rejects a Keep-a-Changelog skeleton -- bare `###
  Added` / `### Fixed` subheadings with nothing under them, the likeliest shape of
  "forgot to write the changelog" -- and matches the heading whatever its depth or
  bracketing, rather than reporting "the section is empty" for a heading it simply did
  not recognise. It sits beside the dirty-tree guard, having first been placed some 200
  lines later, where failing it left the bump commit pushed and the tag on origin: the
  half-released state a preflight exists to prevent.
- **Attribution drift is caught before the tag, and reconciled in preflight.** v0.15.0's
  own release run died at `ci-local.sh` step 10 with the tag already on origin, because
  `THIRD_PARTY_LICENSES.md` no longer matched the dependency graph -- the change that
  added the dependency had gated with `--fast`, which skipped that step outright.
  `--fast` now skips attribution only when the inputs that feed the generated file
  (Cargo.lock, every workspace manifest, about.toml/about.hbs, and the file itself) are
  provably unchanged against merge-base with origin/main, and no base means run;
  `release-local.sh` regenerates the file after the bump and before the tag, landing it
  through the bump's own guards, with `cargo-about` required rather than optional and
  `OAM_NO_AUTO_ATTRIBUTION=1` hard-failing with the crate delta. The file itself now
  carries the three crates the fetch content-encoding fix pulled into the shipped graph
  -- async-compression 0.4.43, compression-codecs 0.4.38 and compression-core 0.4.32,
  all Apache-2.0, adding no new license family -- moving the count from 254 to 257.
  (#124, #123)
- **The release scripts have tests.** `scripts/bump-taps.sh` was 439 lines running during
  a release and pushing commits to two public repos, and it had no coverage of any kind --
  it was not even in the script suite's parse list, so a syntax error in it would have
  reached a release. Fifteen cases now cover it, one per defect found in review, against
  real repos and real pushes, with every assertion reading what origin serves rather
  than the working tree. Three more units followed: the sidecar matrix's boot
  adjudication, which sat inline in the run loop where nothing could call it, is
  extracted as `classifyBoot` beside `classifyCall`; the container memory-limit parser
  is split into `parse_cgroup_limit`, which compiles everywhere rather than only on the
  platform oam is deployed to, so its malformed shapes and v1's unlimited sentinel (as a
  number it would cap the heap at petabytes and claim a container said so) are tested on
  every host the suite runs on; and npm staging gains ten cases, including the guard
  that stops non-executable POSIX packages reaching a registry that forbids
  re-publishing. (#121, #122)

## [0.14.0] - 2026-09-06

A **tty** release: one fix for raw mode, covering the exits that left a terminal
raw, the Unix restore that put back the wrong cooked state, and a failed mode
switch nobody heard about.

### Fixed

- **A program that enabled raw mode could exit and leave your terminal raw.**
  The restore was only a JS `process.on('exit')` listener, and four in-process
  exits never emit `'exit'` (three on Windows, whose console ctrl handler has no
  equivalent re-raise): the stdout/stderr EPIPE bail, the near-heap-limit OOM
  banner, `main`'s fatal sub-code return, and the Unix SIG_DFL re-raise for a
  signal whose JS listener was removed. The runtime now arms its own cooked-mode
  restore -- once, on the first enable that actually flips the terminal, on both
  Unix and Windows -- and the two paths that drained no exit hooks at all, the
  fatal sub-code returns and the signal re-raise, drain them through a new
  `run_exit_hooks` that deliberately leaves the artifact sweep alone. A hard
  kill (SIGKILL) still cannot be covered.
- **On Unix, turning raw mode off restored the terminal's first-ever cooked
  state rather than the one that enable found.** `setRawMode` saved the pre-raw
  `termios` once for the life of the process, so a later disable undid whatever
  had touched the tty since -- a child's `stty`, an inherited-stdio editor,
  another library's `tcsetattr` -- and a redundant disable re-applied that stale
  snapshot. The saved slot is now the mode itself, `Some` while raw and `None`
  while cooked, so every cooked-to-raw transition re-snapshots and an unchanged
  mode does no ioctl at all, which is what libuv does. The same path also
  swallowed a poisoned lock: the save was skipped while raw mode was applied
  anyway, after which every restore took the "nothing saved" arm and reported
  success with the terminal still raw. The lock is now recovered rather than
  dropped, and the snapshot is stored only once the switch has landed.
- **A failed `setRawMode` was silently ignored.** The native op returned a bare
  boolean and the JS side dropped a failure on the floor, so the program went on
  believing it had a raw terminal. The op now returns `uv_tty_set_mode`'s
  contract -- 0, or the negative libuv errno -- and `setRawMode` emits an error
  on the stream carrying `code: 'ERR_SYSTEM_ERROR'`, `errno`, `syscall:
  'uv_tty_set_mode'` and an `info` object, leaving `isRaw` alone as node does.
  **This is a behaviour change**: a failing switch used to report nothing at
  all, and an unhandled `'error'` on the stream now surfaces. The fields are
  filled from the same libuv table `getSystemErrorName` reads -- a deliberate
  divergence from node, which hands `ERR_SYSTEM_ERROR` the bare integer and so
  prints a message of `undefined`s; the `code` that programs branch on matches.

## [0.13.2] - 2026-09-06

An **interactive terminal** release: `readline` and `process.stdin` now behave as
node's do across the sequence every prompting CLI runs -- prompt, close the
interface, prompt again, then switch to raw mode for a TUI. Alongside it,
emptying the node test-suite exclusion backlog turned up nine real bugs in
`util`, `process`, `Buffer` and `node:inspector` that the exclusions had
been hiding.

### Added

- **`TextDecoder` decodes windows-1252, and labels follow the Encoding
  Standard.** Every label but `utf-8`, `utf8` and `unicode-1-1-utf-8` used to
  throw. Labels are now ASCII-whitespace-trimmed and lowercased before lookup,
  and the standard's full alias set for the two encodings oam implements is
  accepted (`ascii`, `us-ascii`, `latin1`, `cp1252`, `x-cp1252` among them). An
  encoding oam does not implement still throws a `RangeError`, now carrying
  node's `ERR_ENCODING_NOT_SUPPORTED` code so feature-detecting callers can
  branch on it. (#113)
- **`url.domainToUnicode()`, and `url.domainToASCII()` through node's host
  parser.** `domainToUnicode` did not exist at all, and `domainToASCII` went
  through `new URL('http://' + domain)`, which parsed `a:80` as a port instead
  of rejecting it. Both now run the domain through the same host parser node
  uses, so `a/b` gives `a`, `a:80` gives the empty string, and `%41` gives `a`.
  (#113)
- **`process.config.variables.v8_enable_i18n_support` is published.** oam links
  full ICU and said nothing about it, so the standard `hasIntl`-style feature
  test read false and Intl-dependent code took a fallback path it did not need.
  (#113)
- **`--abort-on-uncaught-exception` no longer stops the launch.** The flag was
  unrecognized, so a node-shaped launcher that passes it died at argument
  parsing. oam now accepts it and prints one line of stderr saying it is not
  implemented: oam reports the uncaught exception and exits rather than aborting
  at the throw site, so no core dump or debugger break is produced. (#113)

### Fixed

- **A second `readline` interface over `process.stdin` never received its
  answer.** Created after the first was `close()`d -- a trust prompt followed by
  a picker, say -- the second interface starved: the answer echoed on the
  terminal and nothing ran. `close()` paused the input as node's does, but the
  constructor never resumed it, and a paused `Readable` does not restart for a
  new `'data'` listener. The constructor now ends with `input.resume()` as
  node's does, and a closed interface detaches its own `'data'` / `'end'`
  listeners instead of remaining a consumer of the stream. `readline/promises`
  inherits both halves. (#107)
- **On Windows, a program that turned raw mode on right after a prompt saw
  nothing the user typed until they pressed Enter.** `process.stdin`'s `_read`
  issues a blocking read and node's `Readable` refills the moment a chunk is
  pushed, so right after a readline answer the next read is already blocked in
  `ReadConsoleW` under `ENABLE_LINE_INPUT`. A later `setRawMode(true)` flips the
  console mode, but a read already in flight keeps the cooked semantics it was
  issued with and returns only on Enter. oam now cancels the pending read across
  the flip the way libuv's `uv__cancel_read_console` does -- mark the read for
  discard, inject a synthetic Enter with `WriteConsoleInputW`, wait for it to
  settle, restore the cursor -- and re-issues it under the new mode, so no empty
  chunk or stale line reaches JS. The cursor is only put back when the cancelled
  read was a cooked, echoing one, since a raw read's injected Enter echoes
  nothing. As in libuv, type-ahead sitting in the cooked line buffer is lost
  with the cancelled read. (#107)
- **Raw-mode TUIs stair-stepped on macOS and Linux.** `setRawMode(true)` used
  `cfmakeraw`, which clears `OPOST`; with output processing off a bare `"\n"`
  only moves the cursor down, so a program writing one `"\r"` per frame and
  `"\n"` between rows drew a staircase where node drew it straight. libuv
  reserves `cfmakeraw` for `UV_TTY_MODE_IO`; its raw mode inherits `OPOST` and
  forces `ONLCR`, and oam now applies exactly that flag recipe, through
  `TCSADRAIN` in both directions. Windows is unaffected -- only the stdin console
  mode is touched there. (#107)
- **`readline` diverged from node on line terminators, `crlfDelay`, question
  ownership, closed interfaces and abort.** Lines split on `/\r?\n/`, so a lone
  `"\r"` terminator (what a progress bar writes) never surfaced until the next
  `"\n"` arrived, and `crlfDelay` was stored but never consulted; node's
  terminator rule and the option are now both honoured, clamped up to 100 with
  `Infinity` allowed. A line answering a pending `question()` fanned out to
  every `'line'` listener and to a second concurrent question -- the pending
  callback now owns it and no `'line'` event fires. `question()` on a closed
  interface wrote the prompt and hung; it now throws `ERR_USE_AFTER_CLOSE`
  before writing anything, and `rl.closed` is node's own property (undefined
  until `close()` sets it). And the promises variant rejected a `DOMException`,
  so `if (err.code !== 'ABORT_ERR') throw err` crashed; it now rejects node's
  `AbortError` -- an `Error` subclass with `code` `ABORT_ERR` and
  `cause === signal.reason`. (#107)
- **`process.stdin.unref()` did not exist.** Calling it -- the documented way to
  stop stdin holding a process open -- threw a `TypeError` and killed the
  program, exiting 1 where node exits 0. Both `ref()` and `unref()` are now
  present and return the stream, as node's `net.Socket` and `tty.ReadStream` do.
  (#112)
- **`for await (const chunk of process.stdin) break` hung until the pipe
  closed.** Breaking out of an async iterator destroys the stream, and in node
  destroying stdin closes the handle so nothing waits on it. oam's blocking read
  cannot be cancelled, so destroying stdin now retires it instead: the read stops
  counting toward the event loop and the process is free to exit, while the read
  itself completes harmlessly if data ever arrives. (#112)
- **`util.inspect` fired a proxy's traps.** A plain `console.log` of a two-key
  proxy hit its handler 88 times, so formatting a value was observable to that
  value; a revoked proxy threw out of `inspect` instead of printing
  `<Revoked Proxy>`. The walker now reads V8's proxy target and handler slots
  without touching the proxy, which is how node avoids the same problem. (#113)
- **`util.inspect.defaultOptions` was ignored by direct `inspect()` calls.**
  Every option, while the `format` paths honoured it. In the same area,
  `util.inspect(v, null)` crashed and the legacy positional argument form was
  ignored, and `numericSeparator` was stored and reported back but never
  applied. `defaultOptions` is now node's one live options object, read fresh on
  each call, and assigning a non-object to it is `ERR_INVALID_ARG_TYPE`.
  (#113)
- **`util.styleText` emitted ANSI escapes into a pipe.** It colourized
  unconditionally once the stream shape checked out, and `tty.WriteStream`'s
  `hasColors()` was hardcoded to `true` -- so `oam script.js | cat` produced a
  plain assert diff next to a fully escaped `styleText` string, and
  `hasColors(2 ** 24)` answered true on a four-bit terminal. All three surfaces
  now answer from one colour-capability function that honours `FORCE_COLOR`,
  `NO_COLOR`, `NODE_DISABLE_COLORS`, `TERM=dumb` and `isTTY`. chalk,
  supports-color, ora and cli-table3 all read this path. (#113)
- **`process.hrtime(1)` returned `[NaN, NaN]`.** It validated nothing. A
  previous-tuple argument that is not an array is now `ERR_INVALID_ARG_TYPE` and
  an array whose length is not exactly 2 is `ERR_OUT_OF_RANGE`, both checked
  before the clock is read, as node does. (#113)
- **`buf.fill(value, start, end)` threw whenever `end` was before `start`.**
  node treats an empty range as a no-op and returns the buffer untouched. It is
  a no-op here now too, with the value and encoding validation still running
  afterwards -- `fill('a', 4, 1, 'bogus')` is `ERR_UNKNOWN_ENCODING`, not a
  silent success. (#113)
- **The legacy `url` parser mangled hostnames and `url.format()` under-escaped
  what it produced.** The parser matched hostnames against an ASCII allowlist and
  truncated at the first character outside it, so `x://0.0,1.1/` silently became
  host `0.0` with the remainder pushed into the path, and it ran no IDNA at all,
  disagreeing with `new URL()` on an internationalized domain. `url.format()`
  escaped the auth field with `encodeURIComponent` (which throws on a lone
  surrogate) and its path escape table held nine characters, leaving backslash,
  caret and the brace/pipe set unencoded. The parser now runs UTS #46 ToASCII on
  the hostname and throws rather than repairing a result that comes back empty
  or forbidden; `format()` percent-encodes auth itself, substituting U+FFFD for a
  lone surrogate instead of throwing, carries node's full escape table, honours
  `{unicode: true}` by running the host back through ToUnicode, and routes the
  plain-object and legacy `Url` forms through node's own serializer instead of
  assembling them by hand, while a WHATWG-`URL` is re-serialized the way node's
  `bindingUrl.format` does. (#113)

### Changed

- **On Windows, `setRawMode(false)` on a console raw mode was never enabled on
  is now a no-op.** It used to synthesize a target mode from the console's
  current one by adding line, echo and processed input and clearing VT input --
  which on a console that starts with VT input on is a *different* mode, so a
  stray disable silently flipped the console -- and, with the pending-read
  cancel this release adds, would have dropped the read in flight with it.
  libuv's `uv_tty_set_mode` returns early when the tty is already in the
  requested mode, and this now matches. **This is a behaviour change.** (#107)
- **`node:inspector` stops claiming a session it does not have.**
  `Session.post()` called back `cb(null, {})` -- reporting that a CDP command
  succeeded, and handing back `{}` as the debugger's answer, for a command that
  was never dispatched -- and `url()` returned a hardcoded
  `ws://127.0.0.1:9229/0` once `open()` had been called, for a socket nothing
  had bound. `post()` now calls back an `ERR_INSPECTOR_NOT_AVAILABLE` error on
  the same microtask tick, pointing at `oam run --inspect` / `--inspect-brk`,
  and `url()` answers `undefined`, which is what node returns whenever no
  inspector is active. The module and `Session` still construct and connect so
  capability detection works. **This is a behaviour change**: code that read
  `{}` as a real CDP result now sees a failure. (#113)
- **The node test-suite score is 439 of 442 runnable tests, 92.2% of the full
  476-test corpus, up from 429 of 476 (90.1%).** The rate over runnable tests
  falls from 99.5% to 99.3% because eleven tests moved out of the excluded set
  and into the scored denominator rather than staying hidden. The triage
  backlog is now empty: all three remaining failures are marked deliberate with
  a recorded reason and are still counted. `internal/test/binding` is registered
  behind a proxy that throws for anything oam does not genuinely back, the
  receipts now name every skipped and unrunnable test with its reason instead of
  only counting them, and the Windows pass floor moved 429 -> 439. The Linux and
  macOS floors are unchanged and re-measure at release time. (#113)

## [0.13.1] - 2026-09-02

A **TypeScript pipeline** release. One full pass over the path a `.ts` file
takes from disk to a running program -- module resolution, tsconfig handling,
transpile, the three on-disk caches, the type-check daemon and `oam compile` --
fixed 52 findings at once. The headline for anyone writing TypeScript on oam:
stack traces now point at your source rather than at generated code, a warm run
skips the transpiler entirely, graph loading overlaps its own I/O, and a
dependency's `exports` conditions are matched in the order the package declared
them instead of alphabetically.

### Added

- **Stack traces from TypeScript point at your source, not at generated code.**
  oam's codegen reflows every transpiled source (`.ts`, `.mts`, `.cts`, `.tsx`,
  `.jsx`), so every V8 position used to be a line in the transpiled output. The
  loader now emits a source map alongside the transpile and keeps it in a
  process-global registry, and every surface that formats a position consults
  it: uncaught-exception and fatal reports, the code frame (read from the source
  file on disk, since V8 holds only the generated text), `assert.ok`
  call-source extraction, and `err.stack` as read from JS. Warm runs keep the
  fidelity -- transpile-cache and precompile-cache hits register their maps too.
  This is on by default with no flag, which is Node's `--enable-source-maps`
  behaviour. **This is a behaviour change**: oam installs its own
  `Error.prepareStackTrace` at startup, so reading that property before
  assigning it yields a function where Node yields `undefined`. Assigning your
  own still wins, exactly as in Node. (#105)
- **A transpile cache, so a warm run of a TypeScript project skips oxc
  entirely.** Entries live under `<cache dir>/transpile`, content-addressed by
  the source text plus the transpile settings -- which now embed the
  `oxc_transformer` and `oxc_codegen` versions read from `Cargo.lock` at build
  time, so an oam upgrade invalidates them. Writes are temp-then-rename under a
  self-hashed header, so a corrupt entry is a miss; every filesystem failure is
  a silent miss, because transpiling again is always correct. In a scrubbed
  environment with no platform cache dir, the fallback claims a per-user subdir
  of the shared temp dir -- `oam-user-<uid>` created `0700` on Unix, an existing
  one accepted only when it is owned by the current uid -- and disables the
  cache when it cannot do that safely, rather than sharing a world-writable
  directory. `OAM_TRANSPILE_CACHE=0|off|false|no` opts out. (#105)
- **`oam cache info` and `oam cache clean`.** The first prints the bytecode
  cache's directory, entry count and total size; the second deletes it, which is
  safe at any time because the next run recompiles and repopulates. Both are
  registered in the bare-script dispatch list, which a unit test now checks
  against clap's own subcommand list in both directions, so a file named `cache`
  in the working directory never shadows them and `oam --allow-env cache info`
  is not rejected with a space-form hint. Housekeeping also runs on its own now:
  at most once a day, a run sweeps the directory on a background thread,
  removing orphaned `.tmp` files and blobs not written in 30 days. The stamp
  that gates the sweep is written before the walk and doubles as a writability
  probe, so a populated but read-only cache root -- a container image baked with
  a warm cache -- is skipped rather than re-walked in every process. (#105)
- **`compilerOptions.jsx` is honored.** `react` compiles to classic
  `jsxFactory`/`jsxFragmentFactory` calls, `react-jsx` to the automatic runtime,
  and `react-jsxdev` to `jsxDEV()` with `__source`/`__self`. `preserve` and
  `react-native` compile as automatic and are documented as doing so, because
  both exist to leave JSX for a later build tool and there is no later tool when
  the file is about to execute. `extends` now also accepts the TypeScript 5.0
  array form, merged left to right with later entries winning, and an
  absolute-path `extends` resolves. A dependency's own `tsconfig.json` under
  `node_modules` is no longer consulted for a referrer inside it -- neither for
  JSX settings nor for `paths` -- matching what Node and tsc do with a published
  package. (#105)
- **Every tsgo run is bounded and cancellable.** `OAM_TSGO_TIMEOUT_MS` (default
  300000) caps a single run; past it the whole process tree is killed -- the npm
  shim makes the real compiler a grandchild -- and the run reports `OAM-TS0006`,
  with a cancelled run reporting `OAM-TS0007`. Cancellation is pid-safe by
  construction: the new `TsgoHandle` owns the un-reaped child -- a zombie on
  Unix, an open handle on Windows, either way the pid stays reserved -- and
  reaps only after the handle has been taken out of the running state, so a
  cancel can never land on a pid the OS has handed to something else. Warn-mode
  `oam run` now kills its one-shot checker at the deadline instead of orphaning
  it, and how long it waits for that checker after the program exits is tunable
  with `OAM_CHECK_WAIT_MS` (default 10000). `OAM_DEBUG=1` prints why a check
  fell back from the daemon to a one-shot run, which the
  never-worse-than-one-shot contract otherwise swallows. tsgo discovery also
  gained a step and is now documented accurately: `OAM_TSGO`, then the nearest
  `node_modules/.bin` walking up from the target -- new; a project-local tsgo
  was previously reachable only through PATH or the env override -- then PATH.
  The docs claimed a bundled tsgo that never existed. `--version` is probed once
  per binary and gated on a major of 7 or above, so an exit-0 impostor named
  `tsgo` is `OAM-TS0002` rather than a clean check. (#105)
- **`oam daemon stop` can kill an unresponsive daemon.** It used to send
  Shutdown and delete the state file, leaving a wedged daemon running. It now
  falls back to killing the recorded pid when Shutdown goes unanswered, and
  verifies the target by exact image name -- `oam` or `oam.exe`, compared
  case-insensitively against the first `tasklist` CSV field or the file name
  from `ps -o comm=` -- so a recycled pid belonging to something like
  `roaming-helper.exe` is never force-killed. The verify-then-kill window cannot
  be closed with shell tools, so the documentation says best-effort. (#105)
- **A `ts-cold-start` benchmark case.** No case touched TypeScript or any cache
  state before -- `cold-start` runs `console.log('ok')` -- so the claim that
  caching helps TypeScript-heavy servers was unmeasured. The new case times
  wall-clock from process spawn to the first stdout line of a generated
  20-module `.ts` graph, one row per cache state, with node as the reference: as
  published, oam's warm row is 24.38 ms against node's 187.09 ms running the
  same fixture with `--experimental-transform-types` (medians over 20, release,
  windows-aarch64, node v22.22.2). Row labels are verified rather than assumed
  -- a no-cache run that leaves a bytecode blob, or a cold run that leaves none,
  fails the case instead of publishing. The new repeatable `--case <name>` flag
  re-measures one case and merges it into the committed files, so a
  re-measurement never re-stamps numbers that did not move. (#105)

### Fixed

- **A package's `exports` conditions were matched in alphabetical order, so the
  wrong build could load.** JSON objects were held in a `BTreeMap`, which sorted
  the keys: any branch containing `default` resolved to it ahead of `import`,
  `require` or `node`, so `{import, require, default}` with `default` declared
  last loaded the default build for both sides, and `{node, default}` -- the
  shape `yaml` and `tslib` use -- loaded the browser build. tsconfig `paths`
  tie-breaking between equal-length prefixes picked the alphabetically first
  pattern where tsc keeps the first declared. Declaration order is now preserved
  workspace-wide, and every producer of such a map was audited for a dependency
  on the old sorting. (#105)
- **`require()` of a CommonJS-routed `.jsx` fed raw JSX to V8.** The transpile
  gate on the `require()` path is now the loader's own source-kind predicate, so
  a `.jsx` routed CommonJS by the package `type` field, or typeless inside
  `node_modules`, transpiles instead of reaching V8 unchanged. A transpile
  failure now throws a V8 `SyntaxError` carrying the diagnostic code and the
  first diagnostic's `file:line:col`, plus a `(+N more)` count for the
  diagnostics that used to be dropped silently; it was previously a plain
  `Error`. `.ts` and `.mts` stay behind `ERR_REQUIRE_ESM` -- a documented
  divergence from Node 22's type stripping, not a change -- and that error's
  message now points at `import` or a `.cjs`/`.cts` module instead of promising
  a roadmap item. (#105)
- **`oam run` failed on file shapes `oam check` accepted.** Probing now follows
  tsc: `./x.js` tries `x.ts` then `x.tsx`, `./x.jsx` tries `x.tsx`, an
  extensionless specifier tries `ts tsx mts js jsx mjs`, and a directory index
  tries `index.ts`, `index.tsx`, `index.js` and `index.jsx` -- a directory whose
  only index was `index.jsx` used to type-check clean and then die with
  `OAM-MOD0001`. `.cts` also keeps oxc's CommonJS source type, so top-level
  `return`, `new.target` and `await` as an identifier parse the way they do in
  `.cjs`. And `require('fs')` resolves as a builtin before tsconfig `paths` are
  consulted, matching the import side, so a catch-all `"*"` pattern no longer
  shadows a builtin. (#105)
- **`oam install --precompile` wrote nothing over an already-installed tree.**
  It only reached the precompile step inside the fetch-and-extract arm, so a
  package whose `package.json` was already on disk hit the warm-path skip first:
  `oam install` followed by `oam install --precompile`, or the flag over an
  npm-installed tree, could never populate the cache. The pass now runs after
  the install loop and the lifecycle scripts, over every resolved lockfile entry
  whose directory is present, and the per-file freshness header keeps a warm
  re-run at one read, one hash and one open per source file. A file that fails
  to compile also no longer aborts its package -- the failures fold into one
  `OAM-PKG0008` warning naming the count and the first few files. (#105)
- **The precompile cache never invalidated on an oam or tsconfig change.** Its
  key was a hash of the source alone, though the comments claimed otherwise, so
  an oam or oxc upgrade, or a changed `jsxImportSource`, kept serving stale
  output. The key is now the transpile fingerprint plus the source, and each
  entry is a single self-describing artifact written temp-then-rename, replacing
  a non-atomic `.js`/`.hash` pair whose halves could disagree. Writer and reader
  now share one extension set, so `.jsx` is finally covered, and one layout
  anchor, so a nested package gets a slot the reader actually finds. (#105)
- **The type-check daemon could serve a clean result for a tree that no longer
  type-checks.** Its cache is now keyed on tsgo's own program file list
  (`--listFilesOnly`, refreshed on every fill) plus a widened walk over every
  tsc-loadable extension, the tsconfig `extends`/`references` chain, and
  `package.json` and the lockfiles on every ancestor. The measured stale-clean
  repros -- editing a `.js` file covered by `checkJs`, editing a `.ts` file
  under `src/target/` -- now agree with a one-shot check in both directions.
  (#105)
- **An orphaned type-check daemon after a version-skew retire.** `retire()`
  fired Shutdown and returned without confirming the old daemon had exited, so
  an old daemon minutes deep in a check processed that Shutdown afterwards and
  its unconditional cleanup deleted the *new* daemon's state file -- orphaning
  it, so the next check cold-spawned a third. `retire()` now polls for the exit
  (bounded at 2s) and falls back to a kill. The new daemon transport keeps the
  accept thread O(1) alongside it: Checks are queued to a check worker, so Ping
  and Shutdown are answered while a minutes-long check runs, and both the client
  and the server build the recorded tsgo lookup from the canonicalized project
  root, which is what the state file is keyed on. (#105)
- **`oam compile` could exit 0 and hand you a binary that died on its first
  run.** A non-JS entry is now refused up front with a bundle-first hint, and a
  broken JS or ESM entry fails the compile with V8's `SyntaxError` -- either way
  no output file is written, including under a foreign `--carrier`, where entry
  validation used to be skipped entirely. **This is a behaviour change**: a
  compile that previously succeeded on an invalid entry now fails. A foreign
  carrier emits a JS-only payload, because bytecode is bound to the compiling
  binary's V8. Alongside it: `process.argv[1]` now equals `__filename` inside a
  compiled binary (built from the module key, so an 8.3 or symlinked `TEMP` path
  agrees), the embedded blob is produced with eager compilation so every inner
  function's bytecode rides along, ESM blobs are written after the graph
  evaluates so they carry what evaluation compiled, and the success notice is no
  longer double-spaced. (#105)
- **The cache directory could land in the working directory.** With
  `LOCALAPPDATA`, the XDG variable and `HOME` all absent -- a scrubbed container
  environment -- the bytecode cache root and the daemon's state root both fell
  back to `"."`; they now fall back to the system temp directory, and an empty
  variable counts as unset rather than as a path. (#105)
- **Loader warnings now reach every command.** A malformed tsconfig or an
  `extends` cycle used to be silent -- swallowed by a parse `ok()?` and by the
  depth limit that gave up quietly -- and a bare-package `extends` printed raw
  `eprintln` prose. All three are now ODIF diagnostics (`OAM-MOD0009` and
  `OAM-MOD0008`, both in `oam explain`), drained in the epilogue of every
  command that constructs a runtime: `oam run`, `oam check`, `oam test`, the
  REPL, `-e`, and the embedded, install and bare-script paths. The drain is also
  registered as an exit hook, so a script calling `process.exit()` still prints
  them. (#105)
- **`dns` NAPTR records came back with their properties in the wrong order.**
  Preserving declaration order let the literal key order reach JS verbatim. The
  old alphabetical sort happened to match Node for MX (`exchange`, `priority`)
  and for SRV (`name`, `port`, `priority`, `weight`) -- those two literals were
  reordered so the flip preserves that output -- but not for NAPTR, whose Node
  order is `flags`, `service`, `regexp`, `replacement`, `order`, `preference`.
  **This is a behaviour change** for anything that reads a NAPTR object
  positionally. SRV's order was verified against a live node v22.22.2; NAPTR's
  comes from `cares_wrap.cc`'s `ParseNaptrReply`. (#105)
- **A file added under an out-of-root include directory could be served a stale
  clean check result.** The daemon fingerprint stamped the parent directories of
  listed files outside the project root with `(mtime, size)`. On Linux,
  directory timestamps come from the kernel's coarse per-jiffy clock and an ext4
  directory's size sits at 4096 regardless of what it holds, so a file created
  in the same tick as that directory's previous modification left the stamp --
  and therefore the fingerprint -- unchanged. Those directories are now stamped
  by an FNV hash over their sorted entry names plus the entry count, so a
  create, delete or rename invalidates regardless of filesystem timestamp
  granularity; per-file edits stay covered by the listed files' own
  `(mtime, size)` stamps. The same coarse clock also flaked the regression test
  on the release Linux leg. (#106)

### Changed

- **Module-graph preparation overlaps with a prefetch worker pool.** Graph
  loading ran the filesystem read, the oxc transpile, the SHA-256 and the V8
  compile strictly serially on the isolate thread. As each module's import
  requests are resolved, not-yet-loaded paths now go to a small pool
  (`min(4, cpus)`, spawned lazily per graph load) that runs the host load and
  the bytecode-cache probe ahead of the loop and parks the results for the loop
  to consume. V8 compile and instantiate stay on the isolate thread; builtins,
  JSON and CommonJS interop still load inline, because they need the isolate.
  Error surfacing is byte-identical to the serial path -- a parked error is
  returned exactly when its path is popped, one for a path never popped is
  dropped, and a worker panic re-raises on the isolate thread at the same point.
  In-flight paths are deduped, so a diamond import loads once. On the new
  `ts-cold-start` case the no-cache and cold rows fell about 22 percent when
  this landed (39.32 to 30.71 ms and 37.39 to 29.24 ms, medians over 20 on
  windows-aarch64). `OAM_GRAPH_PREFETCH=0|off|false|no` restores the serial
  path. (#105)
- **The bytecode-cache key is derived once per compile.** `key_for()` folds the
  V8 version tag, a new oam-side format stamp and -- for the CommonJS wrapper --
  its parameter list, which are shapes V8's own blob sanity check cannot see,
  and both consume sites hand that one key to load and store instead of hashing
  the source twice per miss. The key is not derived at all when the cache is
  disabled. (#105)
- **Superseded bytecode seeds are recorded on disk.** A compiled binary's
  embedded blob is still consulted before disk, since it ships inside the binary
  and is therefore the trusted copy. When V8 rejects it, the consume site now
  writes a `<key>.seedrej` tombstone next to the refreshed blob, so later
  processes prefer the disk copy for that one key instead of re-rejecting and
  re-writing on every run. The tombstone is written only on the rejection path,
  so a compiled binary whose seed is accepted still performs zero cache writes.
  (#105)
- **Loader warnings are ODIF diagnostics, and the human renderer labels
  severity.** A tsconfig problem now surfaces as ODIF JSONL on stderr under
  `--json`, like every other diagnostic, instead of prose from an `eprintln`,
  and as one warning line otherwise; warnings are deduped per path. The renderer
  prints `warning[...]` and `info[...]` instead of labelling every diagnostic
  `error[...]`. (#105)

## [0.13.0] - 2026-08-31

A **`FileHandle`** release, with the N-API audit thread closing behind it.
`fsPromises.open()` now returns all twenty of Node v22's `FileHandle` members --
thirteen of them used to be a `TypeError` -- and wiring up its stream factories
turned up two `fs.createReadStream` divergences reachable from the plain path
API. On the native-addon side: an ABA hole in `napi_ref`, a moved-pointer defect
that a new miri-checked model found on main, the N-API layer becoming a cargo
feature you can compile out, and `--allow-addons` finally enforcing the grant it
had only ever reported.

### Added

- **`fsPromises.open()` returns a complete `FileHandle`.** `appendFile`,
  `chmod`, `chown`, `truncate`, `sync`, `datasync`, `readv`, `writev`,
  `utimes`, `createReadStream`, `createWriteStream` and `readLines` were all
  absent, so calling any of them was a `TypeError`. A closed handle now also
  fails in Node's shape rather than reaching the descriptor: the promise-
  returning methods reject with `EBADF` plus the per-method `syscall`, while
  the three stream factories match Node's other shape there -- a synchronous
  `ERR_OUT_OF_RANGE` on fd -1. (#99)
- **`FileHandle.readableWebStream()`**, the twentieth and last member of Node
  v22's `FileHandle`. It does not take ownership: `autoClose` defaults to
  `false`, so a fully drained stream leaves the descriptor open -- the opposite
  of `createReadStream`. The handle is locked to the stream for life on the
  first call. A BYOB reader is not supported (oam's web-streams layer has no
  byte controller, so a default reader is returned); see
  `docs/node-divergences.md`. (#102)
- A miri-checked model of the N-API pointer disciplines runs as its own gate
  step, so the aliasing claims behind the addon layer are machine-checked
  rather than argued. It found the `load_addon` defect below. (#88)

### Fixed

- **A `FileHandle` described whatever file currently sat at its path, not its
  own descriptor.** `fh.stat()` called the path-based `stat` rather than
  `fstat`, so after the file was renamed or unlinked it reported a different
  file's metadata, or failed outright -- open-then-unlink being an ordinary
  temp-file pattern. It now stats the descriptor. (#99)
- **`fs.createReadStream`'s `start` option never seeked.** It fed the byte
  budget while every read still came from the cursor, so `{start: 6, end: 10}`
  returned the file's first five bytes instead of the requested window. The
  same code also closed the descriptor at EOF under `autoClose: false`, taking
  ownership the caller had explicitly withheld. Both are reachable from the
  plain path API, not just from a `FileHandle`. (#99)
- **A deleted `napi_ref` could silently resolve to a different live reference.**
  Handles were validated by comparing addresses, so once the allocator reused a
  freed entry's memory a stale handle passed the check and returned whichever
  reference now sat there -- 40 times in 5,000 create/delete cycles when
  measured. A handle is now an index plus a generation counter, and a stale,
  forged, or cross-environment handle is refused instead of resolved. (#91)
- **Loading a native addon used a pointer that a move had invalidated.**
  `load_addon` derived the `napi_env` pointer from a `Box` and then moved that
  box into the registry, which invalidates pointers derived from it; the
  pointer is now derived after the move, matching what `napi_create_function`
  already did. (#88)
- **A remote build could pack gigabytes of unrelated content and then fail with
  nothing printed.** The source sync tarred the working tree against a
  hand-maintained list of `--exclude` patterns duplicated across both platform
  legs, four of the five `./`-anchored, so those four matched only the
  top-level copy: agent git worktrees carrying their own `target/` were packed
  whole, one run producing an 11.6 GB tarball from a ~1.6 MB tree and dying 35
  minutes in without an error message, because the linux leg's sync step had no
  error handling. The sync now ships what `git ls-files --cached --others
  --exclude-standard` reports, from one shared definition rather than two
  copies -- 11.6 GB became 1.72 MB and 35+ minutes became 0.94s -- under a
  tarball size ceiling (`OAM_SRC_TARBALL_MAX_MB`, default 200MB) that names the
  biggest paths on a breach, with explicit failure messages on every step of
  both legs. (#104)

### Changed

- **`--allow-addons` is now enforced, not merely reported.** It maps to the
  `ffi` permission, which nothing consumed: the grant was visible to
  `process.permission.has('ffi')` and then ignored, so under `--permission`
  *without* `--allow-addons` an addon still loaded whenever
  `OAM_ENABLE_NATIVE_ADDONS` was set. **This is a behaviour change**: such a
  run is now refused with `ERR_ACCESS_DENIED` (`permission: 'Addon'`). The
  environment variable and the grant are independent and both must pass --
  an env var cannot widen a permission the caller withheld. (#96)
- **The N-API layer can be compiled out.** `oam_engine`'s `napi` feature is on
  by default, so an ordinary build is byte-for-byte unchanged; building without
  it leaves the 132-symbol Node-API surface out of the binary entirely, and
  `require()` of a `.node` file then reports the new `OAM-NATIVE0002` rather
  than advising an environment variable that build cannot honour. (#90)
- The POSIX filesystem and child-process paths use `rustix` and owned file
  descriptors in place of hand-written `libc` calls, and 16 further `unsafe`
  constructs that asserted nothing the compiler was not already proving are
  gone. Behaviour is unchanged; `oam_core`'s audited `unsafe` surface halves.
  (#92, #89)
- **The miri gate now fails closed.** Its teeth loop asserts that the three
  models of shapes that were real bugs are still rejected, but it only tested
  for a non-zero exit -- an ordinary assertion failure or an interpreter crash
  satisfied it just as well, and the output that would have shown the
  difference was discarded. It now requires an undefined-behaviour diagnosis in
  the captured output and prints that output on a miss. The first version of
  that match ran `printf | grep -q` under `pipefail`, which SIGPIPEs on a large
  report -- measured returning 141 at 200KB -- so the match is now a bash
  pattern, which spawns no subprocess. (#94, #95)
- **The N-API-off build is gated, and the gate's own decision logic is
  tested.** The `--no-default-features` configuration #90 introduced had been
  verified by hand once and appeared nowhere in the scripts or tests; it now
  has its own gate step that builds it, clippies it with `-D warnings`, and
  runs the feature-off tests. The miri verdict logic moved into
  `scripts/lib/miri-gate.sh` so `scripts/test-scripts.sh` can drive it with
  captured output even on boxes with no nightly toolchain, and it now
  distinguishes a run in which zero models executed from a clean one. Two
  node-differential conformance cases and e2e coverage for the `napi_ref`
  handle table and the addon gates land with it; the `unsafe` ceiling moves
  `oam_engine` 597 -> 602 for five test-only blocks, and the test addon 68 ->
  70 for its new `readRefInt` export. (#97)

## [0.12.1] - 2026-08-27

An **N-API** release. Everything below is on the native-addon surface, which is off by
default and reachable only with `OAM_ENABLE_NATIVE_ADDONS=1` -- an ordinary run is
untouched. Most of it lands on the reference API (`napi_create_reference` and its
family), which rejected nothing beyond a null pointer before this release.

### Fixed

- **A deleted `napi_ref` was still dereferenced, so an addon read and wrote freed memory
  and was told it had succeeded.** `napi_delete_reference` drops the entry, but
  `napi_get_reference_value`, `napi_reference_ref` and `napi_reference_unref` each cast
  the caller's handle straight back to a reference behind a null check alone -- an
  ordinary addon lifecycle mistake became a use-after-free that returned `napi_ok`. All
  three now look the handle up in the env's own reference table first and answer
  `napi_invalid_arg` (1) when it is not there: already deleted, never created by this
  runtime, or created in a different env, since two addons loaded into one process have
  separate tables. One limit remains, and is written down rather than implied: the lookup
  compares addresses, so a stale handle whose entry has been freed and replaced at the
  same address resolves to a different live reference -- an aliasing bug rather than a
  use-after-free, and closing it needs handles that carry a generation counter. (#86;
  recorded in `docs/node-divergences.md` as divergence 22 in #87)
- **`napi_create_reference` with a null out-pointer left a reference nothing could
  reach.** It pushed the entry first and discovered the null `result` afterwards, so the
  reference sat in the env unreachable by any handle until the env itself dropped. It now
  validates up front and creates nothing, which is what Node does. (#86)
- **`napi_get_last_error_info` always answered "nothing failed".** It handed back a
  permanently zeroed static -- `error_code: 0` (`napi_ok`) and a null message, whatever
  had just gone wrong. That was harmless while nothing returned an interesting failure;
  the handle validation above changed it, so an addon following Node's documented "the
  call failed, ask why" path was told nothing had failed, exactly while debugging
  something that had. `NapiEnv` now carries its own `napi_extended_error_info` slot --
  per-env, as in Node, which is what makes handing back a pointer to it sound, since it
  lives as long as the env the addon is calling through -- and the reference family
  records a status plus a message naming the call and the reason, clearing it on success.
  Only that family records: after a failure anywhere else the slot still holds whatever
  the last recording call left, so treat a message as authoritative only when it names
  the call you just made. Documented as divergence 23. (#87)
- **Two N-API entry points handed out overlapping exclusive borrows of the same V8
  scope.** `napi_define_class` and `napi_define_properties` each derived a
  `&mut PinScope` from the env and then called `napi_create_function`, which derived a
  second one from that same unchanged field; creating it invalidated the first, which
  both callers went on using. Both sit on the ordinary addon registration path, and
  `napi_define_class` repeats it once per method descriptor. `env_scope` now hands out a
  shared `&PinScope` instead: nothing in this layer writes through the scope (V8's
  interior mutation goes through a `Cell` inside the handle scope, which is sanctioned
  through a shared reference), so the two borrows coexist rather than invalidating each
  other. Not one line of control flow changed. (#84)

### Changed

- **`napi_delete_reference` now reports a handle it does not recognise instead of
  succeeding silently.** It was the one function in the reference family that validated
  nothing past a null pointer: it retained the entries that did not match and returned
  `napi_ok` for any non-null handle, so a double-delete or a foreign handle looked
  successful and hid the addon's own lifecycle bug -- while read, ref and unref on that
  very handle correctly answered `napi_invalid_arg`. It now reports whether anything was
  actually removed. **This is a behaviour change**, and a deliberate divergence rather
  than parity: Node treats an invalid `napi_ref` as undefined behaviour. Documented as
  divergence 22. (#87)

## [0.12.0] - 2026-08-23

A **Node-fidelity** release, concentrated on three surfaces that were quietly
answering with the wrong value rather than failing: the errno tables behind
`util.getSystemErrorName` and `os.constants.errno`, which stop being one
transcribed literal shared across platforms and come from the host -- the
compiler on unix, a differentially-pinned table on Windows; `process`'s identity
properties, which gain Node's own descriptors; and the DNS layer, which learns
the distinctions c-ares makes -- a name that does not exist versus a name that
carries no record of the type you asked for. Windows also gets a packaging fix
worth its own line.

### Added

- **`process.versions` now reports `ada`.** It is the one Node dependency name
  oam withheld by mistake rather than by principle: oam parses every URL with
  ada, the same C++ WHATWG parser Node reports under this key. The value is the
  version of the *vendored* C++ ada, read at build time from the pinned
  `ada-url` crate's own `deps/ada.h` -- not the Rust binding crate's version,
  which moves independently and would put a number under a Node-owned name that
  means something else. (#64)
- **`util.getSystemErrorMessage()`**, which was absent entirely, so calling it
  was a `TypeError`. It reads the same per-platform libuv table as
  `getSystemErrorName`. (#64)

### Fixed

- **`oam.exe` no longer needs the VC++ redistributable.** The Windows builds link
  the static CRT, so a fresh machine can run a downloaded binary without first
  installing Microsoft's runtime. (#66)
- **`util.getSystemErrorName()` decoded errno numbers against an invented
  table.** The pair carried a private `-1..-28` sequence that matched no
  platform, so on Windows `getSystemErrorName(-4058)` answered "Unknown system
  error -4058" for a plain `ENOENT`, and on Linux all but one of the codes past
  `ENOENT` were wrong (`EACCES` is -13 there, not -3; only `EBADF` happened to
  land on its real number). The replacement built in #64 was transcribed and
  held Linux values, so on macOS a real `ECONNREFUSED` (-61) decoded to
  `ENODATA`. The tables now come from the host: on unix from `libc` through a
  new native, so the compiler resolves each constant for the target and a name
  the target does not define is a build error rather than a wrong number at
  runtime; on Windows from the fixed MSVC CRT and Winsock set, pinned
  key-for-key against real Node by a conformance case. The libuv table is then
  derived by libuv's own rule rather than transcribed a third time, and the
  Windows table oam stamps onto `err.errno` is filled out to libuv's full
  85-entry set so the two round-trip. (#64, #72, #76)
- **`os.constants.errno` shipped Linux's numbers everywhere, and was incomplete
  on Windows.** One Linux-valued literal served every host, so on macOS
  `EAGAIN` read 11 where the host says 35 and `EADDRINUSE` read 98 where it is
  48. On Windows the table went from 79 to 134 entries: 44 values corrected, the
  58 `WSA*` names it never had added, and the three names Windows does not have
  (`EDQUOT`, `EMULTIHOP`, `ESTALE`) dropped. Two follow-ups finished it: the
  native blob is split so `os.constants.errno` publishes only the key set Node
  publishes on that platform (79 names on POSIX) while the libuv table keeps the
  wider set it needs, and the `WSA*` block is no longer emitted alphabetically
  -- Node orders it ascending by value, which is observable through
  `Object.keys`, `JSON.stringify` and `util.inspect`. (#72, #76, #79)
- **Mutating the map from `util.getSystemErrorMap()` corrupted later lookups.**
  The returned `Map` was fresh, so `set`/`delete`/`clear` on it were contained,
  but the `[code, message]` arrays inside it were the memoized table's own --
  so `map.get(k)[0] = x` rewrote what `getSystemErrorName` and
  `getSystemErrorMessage` returned for the rest of the process. Node builds
  fresh arrays per call; oam now does too. (#75)
- **`util.getSystemErrorName()` rebuilt its whole table on every call, and
  validated nothing.** 200k lookups took 12704ms against Node's 19ms; memoized,
  62ms. Neither it nor `getSystemErrorMessage` checked its argument, returning
  "Unknown system error [object Object]" where Node throws
  `ERR_INVALID_ARG_TYPE` for a non-number and `ERR_OUT_OF_RANGE` for zero, a
  positive, `NaN` or a non-integer. Both now match, including Node's digit
  grouping for large values and its rendering of `-0` as "-0". (#72, #76)
- **A DNS name that exists but carries no record of the requested type looked
  like a name that does not exist.** Every hickory "no records" outcome
  collapsed to `ENOTFOUND`; the mapping is now NXDOMAIN to `ENOTFOUND` and
  NOERROR to `ENODATA`, which is what c-ares -- and therefore Node -- reports,
  with any other response code staying `ESERVFAIL`. A follow-up made the split
  hold on hosts that have a DNS search list: c-ares treats a name carrying at
  least `ndots` dots (1 by default) as already qualified and reports that one
  answer, while hickory walked the search list on any "no records" outcome and
  let the last bogus attempt decide the error -- so a NODATA answer surfaced as
  `ENOTFOUND`. Such a name is now queried absolutely; a name with fewer than
  `ndots` dots still goes through the search list. (#80, #83)
- **Every DNS name came back with a trailing root dot and with IDNA labels
  decoded.** hickory's `Display` does both and Node's does neither, so
  `resolveNs("xn--p1acf")` returned decoded Cyrillic where Node returns
  `ns1.nic.xn--p1acf`. All nine name sites -- CNAME, `MX.exchange`, NS,
  `SRV.name`, `SOA.nsname`, `SOA.hostmaster`, PTR, `NAPTR.replacement` and
  reverse -- now route through one ASCII helper, and the root name renders as
  the empty string rather than `"."`, matching what Node reports for a null MX.
  (#80)
- **`dns.getServers()` returned an empty array.** The nameserver list is now
  captured when the resolver is built, from the same configuration the queries
  actually go to, rather than echoing back whatever a caller had handed
  `setServers`. Duplicate addresses are dropped and order is preserved. (#80)
- **`dns.resolveCaa()` reported the wrong shape.** It emitted
  `{critical, issue: <tag>, value: <value>}`: the tag was written where the
  value belongs, the key was hardcoded to `issue` regardless of the record's
  real tag, and a `value` key Node does not have was added -- so a domain
  publishing an `iodef` record alongside `issue` records reported all of them as
  `issue`. Node's shape is `{critical, <tag>: <value>}`, with the tag itself as
  the key. `critical` is also the wire flags octet rather than a boolean, so a
  record with the RFC 6844 Issuer Critical bit set reads 128. (#81)
- **DNS errors carried no `errno`, `syscall` or `hostname`.** A handler doing
  `err.syscall === "queryA"` never matched, and one logging the decoded errno
  printed "Unknown system error undefined". Failures from `dns.lookup`, the
  `dns.resolve*` family and `dns.reverse` -- callback and promise forms alike --
  are now reshaped into Node's error, message included. `errno` is stamped on
  the `dns.lookup` path only, which is Node's own split: its `resolve*` and
  `reverse` errors come from c-ares and carry no libuv number either. (#72)
- **`child_process.execFile()` called its callback twice on a missing binary**,
  the second time with the numeric libuv errno in `err.code` (-4058 on Windows,
  -2 elsewhere), so `err.code === 'ENOENT'` took the wrong branch. Node guards
  at the callback layer and oam now does too; `err.cmd` is stamped as well.
  (#72)
- **Assigning to `process.argv` or `process.execPath` threw in ESM and silently
  vanished in CJS.** Both were getter-only, which broke the CLI test harnesses
  that rely on the assignment. They keep their lazy read and gain setters.
  `process.argv0` is independent of the `argv` setter, as Node's is -- it is a
  bootstrap snapshot, so retargeting `argv` leaves it reporting the name the
  process was invoked under. (#72, #76, #79)
- The release pipeline reclaims builder disk before a build, and two IAP tunnel
  warnings that fired on every remote build are fixed. `scripts/` is now covered
  by its own gate. (#67)

### Changed

- **`process`'s identity properties are read-only.** `version`, `versions`,
  `arch`, `platform`, `release`, `config`, `pid`, `features` and each member of
  the `versions` and `release` bags were plain writable properties, so a package
  that assigned to `process.platform` corrupted it for everything loaded after
  it; Node defines them with `writable: false`, where such an assignment
  silently no-ops in CJS and throws in ESM. **This is a behaviour change** for
  code that assigned to them, and it is not `Object.freeze`: the objects stay
  extensible and mostly configurable, which is Node's shape. `process.features`
  also picks up Node's key order, which `JSON.stringify(process.features)`
  depends on. (#64, #72)
- **`os.constants` members are read-only and non-configurable**, as Node's are.
  `errno` was the first bag to get this; `signals` and `priority` were left
  writable and configurable by that change and now match too. (#72, #79)
- **`dns.setServers()` now throws instead of quietly doing nothing.** oam's
  resolver is process-global, so recording a caller's list and then querying the
  system servers anyway was the worst outcome available -- a `Resolver` pointed
  at a blackhole address returned real records, and an answer from the wrong
  nameserver is indistinguishable from a correct one. **This is a behaviour
  change**: both `dns.setServers` and `Resolver.prototype.setServers` raise an
  error with `code: 'ENOSYS'`, sending the caller to its own fallback. It is a
  deliberate divergence from Node and is documented as one. (#80)
- `--allow-worker` is documented as no longer implying `--allow-child-process` --
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

[Unreleased]: https://github.com/YawLabs/oam/compare/v0.15.0...HEAD
[0.15.0]: https://github.com/YawLabs/oam/compare/v0.14.0...v0.15.0
[0.14.0]: https://github.com/YawLabs/oam/compare/v0.13.2...v0.14.0
[0.13.2]: https://github.com/YawLabs/oam/compare/v0.13.1...v0.13.2
[0.13.1]: https://github.com/YawLabs/oam/compare/v0.13.0...v0.13.1
[0.13.0]: https://github.com/YawLabs/oam/compare/v0.12.1...v0.13.0
[0.12.1]: https://github.com/YawLabs/oam/compare/v0.12.0...v0.12.1
[0.12.0]: https://github.com/YawLabs/oam/compare/v0.11.0...v0.12.0
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
