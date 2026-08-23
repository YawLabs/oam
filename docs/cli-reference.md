# CLI and environment reference

`oam --help` and `oam <command> --help` are authoritative. This page is the
overview, plus the environment variables, which `--help` does not list.

## Commands

| Command | What it does |
|---|---|
| `oam run <file>` | Run a JS/TS file. TypeScript is type-checked **concurrently** — execution never waits on the checker. |
| `oam test` | Run `*.test.*` / `*.spec.*` / `*_test.*`, each file in a fresh isolate. Import `oam:test`. Does not block on types. |
| `oam repl` | Interactive typed REPL. Also the default with no subcommand. |
| `oam check` | Type-check a file or project with tsgo (TypeScript 7 native). |
| `oam daemon` | Inspect or stop the per-project type-check daemon. |
| `oam mcp` | Serve oam's introspection to coding agents over MCP (stdio). E.g. `claude mcp add oam -- oam mcp`. |
| `oam serve <file>` | `oam run` with `PORT`/`HOST` set from `--port`/`--host`. |
| `oam install` | Install from the lockfile (the `npm ci` equivalent). |
| `oam trust` | Manage the trust list for package lifecycle scripts. |
| `oam compile <file>` | Embed a **pre-bundled** JS file into a standalone executable. Bundle it yourself first (esbuild/rollup); this does not bundle. |
| `oam self-update` | Re-run the canonical installer from oamjs.org, verifying against the published `SHA256SUMS`. |

Global: `--json` emits machine-readable ODIF JSONL on stderr instead of
pretty-printed errors — the form agents should consume.

## Node compatibility flags

oam accepts the Node flags it implements, so existing invocations keep working.
Notable ones:

| Flag | Notes |
|---|---|
| `--permission` | Denies everything, then grants back with the `--allow-*` flags below. |
| `--allow-fs-read=<paths>` / `--allow-fs-write=<paths>` | `*` for everything, otherwise a comma-separated allow-list. Both take the list with `=`; neither has a bare form. |
| `--allow-net[=<hosts>]` / `--allow-env[=<names>]` | Bare grants everything; the `=` form takes a comma-separated allow-list. |
| `--allow-child-process` | Spawning **and** `process.execve`, which replaces the image. |
| `--allow-worker` | Starting a `worker_threads` Worker or an `oam.fork()` isolate. Does **not** imply `--allow-child-process` — a child isolate inherits the parent's permissions, so it cannot spawn unless the parent could. |
| `--allow-addons` | Loading native addons. |
| `--experimental-vm-modules` | Enables `vm.SourceTextModule`. |
| `--expose-internals` | Resolves `internal/*` from the builtin registry. |
| `--expose-gc`, `--no-warnings`, `--no-deprecation`, `--pending-deprecation` | As in Node. |
| `--env-file=<path>`, `--env-file-if-exists=<path>` | As in Node. |
| `-e` / `--eval`, `-p` / `--print`, `-pe` | As in Node, including the bundled form. |

## Environment variables

### Runtime

| Variable | Effect |
|---|---|
| `OAM_ENABLE_NATIVE_ADDONS=1` | Enable N-API addon loading. **Off by default and alpha**: an addon compiled against `node.exe` can deadlock the OS loader inside oam, before any oam code runs, so the default is a clean throw that lets a package's JS fallback take over. |
| `OAM_MAX_HEAP_MB` | Cap the V8 heap. Set this to match a container memory limit. |
| `OAM_MAX_BODY_BYTES` | Aggregate cap on queued HTTP request-body bytes across all in-flight requests (default 512MB). Past it, excess uploads are shed rather than buffered. Per-request backpressure is the first line of defence; this bounds the total once concurrency is high. |
| `OAM_CODE_CACHE` | Control V8 code-cache reuse across runs. |
| `OAM_CACHE_DIR` | Where oam keeps its caches. |
| `OAM_IO_URING` | Opt into the Linux io_uring FS path. Off by default — it benchmarked as not a win. |
| `OAM_EXPERIMENTAL_VM_MODULES` / `OAM_EXPOSE_INTERNALS` | Env equivalents of the flags above; set by the CLI, readable by the loader. |

### Type checking

| Variable | Effect |
|---|---|
| `OAM_TSGO` | Path to the tsgo binary, when it is not the bundled one. |
| `OAM_DAEMON_IDLE_MS` | How long the type-check daemon stays warm before exiting. |

### Install and update

| Variable | Effect |
|---|---|
| `OAM_VERSION` | Pin the version the installer fetches (e.g. `v0.8.0`). |
| `OAM_INSTALL_DIR` | Install target. Default `~/.oam/bin`, or `%LOCALAPPDATA%\oam\bin`. |
| `OAM_INSTALL_BASE` | Asset base URL, for a mirror or CDN. Default is GitHub Releases. |
| `OAM_SELF_UPDATE_URL` | Override the installer URL `oam self-update` fetches. |
| `OAM_GH_API` | GitHub API base, for GitHub Enterprise. |
| `GH_TOKEN` / `GITHUB_TOKEN` | Needed while the repo is private — unauthenticated asset URLs 404. In a pipeline put it on `sh`, not on `curl`: `curl -fsSL … \| GH_TOKEN=… sh`. |
| `OAM_IGNORE_SCRIPTS` | Skip package lifecycle scripts during install. |

### Diagnostics

| Variable | Effect |
|---|---|
| `OAM_NAPI_TRACE` | Trace N-API calls. |
| `OAM_CRASH_SELFTEST` | Force a crash path, for testing the fatal reporter. |
| `OAM_DEP_VERSIONS` / `OAM_GIT_SHA` | Build-time provenance, surfaced in `process.versions`. |
| `OAM_CLUSTER_WORKER` | Set by oam in cluster workers; not something you set yourself. |

## Related

- [why-oam.md](why-oam.md) — how oam compares to Node, Deno and Bun, and when to
  pick one of them instead.
- [node-divergences.md](node-divergences.md) — every place oam knowingly differs
  from Node, and why.
- [../BENCHMARKS.md](../BENCHMARKS.md) — numbers, and how to reproduce them.
