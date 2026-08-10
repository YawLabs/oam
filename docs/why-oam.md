# Why oam, and when not

Straight comparison against the runtimes you would otherwise pick. Numbers here
are reproducible — see [BENCHMARKS.md](../BENCHMARKS.md) for how to run them
yourself on your own hardware.

## The short version

oam is for **running TypeScript and MCP servers**. If that is not what you are
doing, one of the other three is probably the better tool, and the rest of this
page says so specifically.

## vs Node.js

Node is the compatibility baseline, and oam is measured against it rather than
against a marketing claim: a vendored subset of Node's own test suite runs on
every release. **429/431 on windows-aarch64, 438/440 on macos-aarch64, 439/441
on linux-x86_64.** Both remaining failures on every platform are deliberate and
documented in [node-divergences.md](node-divergences.md).

Where oam differs:

- **TypeScript runs directly**, including `.tsx`/`.jsx`, with no build step and
  no loader hook.
- **Faster to start.** Cold start is roughly half Node's, and for an MCP server
  the gap is much wider because the protocol surface is built in rather than
  installed from npm.
- **A single binary.** No `node_modules` for the runtime itself, no version
  manager.

Where Node is still the right answer:

- **Native addons.** oam's N-API support is alpha and off by default, because an
  addon built against `node.exe` can deadlock the OS loader inside a different
  host. If your dependency tree has a mandatory `.node` binary with no JS
  fallback, use Node.
- **Anything needing the full ecosystem surface.** oam covers the `node:`
  builtins that real packages use; it is not a complete reimplementation, and
  the divergence list is public precisely so you can check before you commit.
- **Long-lived production services you already operate.** oam is beta, and there
  is no LTS yet.

## vs Deno

Deno and oam agree on a lot: single binary, TypeScript without a build step,
permissions as a first-class idea. The differences that matter:

- **npm compatibility is the default here, not a compatibility layer.** oam
  resolves against an ordinary `node_modules` and speaks CommonJS natively.
- **oam's permission model is Node's** (`--permission` with `--allow-fs-read`,
  `--allow-child-process` and friends), so it matches the flags your existing
  tooling and docs already use.

Deno is the better choice if you want its standard library, `deno deploy`, or
its own module ecosystem. Those are not goals here.

## vs Bun

Every figure in this section comes from the committed run at commit `acec008`,
release profile, host windows-aarch64. The full table is in
[`BENCHMARKS.md`](../BENCHMARKS.md) and the raw samples are in
[`bench/results.json`](../bench/results.json).

Bun is faster than oam at several things and the benchmark table says so
plainly — it wins `url-parse`, `http-throughput` and `crypto-hash` on the same
hardware, the last of them by nearly 6x. If raw single-process throughput is
what you are optimising, benchmark Bun. (Node, not Bun, is the one to beat on
`fs-read`, and it does — narrowly.)

oam wins where the workload is **spawn-heavy and short-lived**, which is exactly
the MCP sidecar shape: `mcp-cold-start` 35ms vs Bun's 483ms, `mcp-idle-rss` 25MB
vs 98MB, `mcp-first-call-latency` 0.34ms vs 5.04ms. A broker that starts a dozen
sidecars pays those costs a dozen times.

Treat the spawn-bound rows as the softest numbers in the set. `cold-start`,
`mcp-cold-start` and `mcp-first-call-latency` are dominated by how fast the OS
can get an executable running, which on-access malware scanning can multiply by
5-10x — and the harness stages oam's binary to a temp directory and execs it
once before timing, so oam absorbs that cost up front where node and bun pay it
on every iteration. On a machine with such a scanner active, those three rows
flatter oam. Measure on your own hardware before you rely on the margin.

Bun is also a bundler, test runner, and package manager. oam is not trying to be
those.

## The MCP case specifically

oam is built to host MCP servers, which is an unusual workload: many
short-lived processes, each idle most of the time, each needing to answer its
first request quickly. That shape rewards low cold-start and low idle RSS over
peak throughput, and it is why the numbers above are the ones that matter.

`oam:mcp` is a built-in module, so a server does not install an SDK to speak the
protocol. Yaw MCP runs its sidecars on oam by default, and every oam release is
gated on a matrix that boots each one and calls a real tool
(`scripts/mcp-sidecar-matrix.mjs`).

## Honesty notes

- **Beta.** Breaking changes before 1.0 are still possible and are called out in
  the changelog. There is no LTS yet.
- **The conformance number has a denominator.** 99.5% is *pass over tests that
  ran*; the corpus is a subset of Node's suite, and
  [node-divergences.md](node-divergences.md) explains what is excluded and why.
- **Binaries are unsigned**, and checksummed against a published `SHA256SUMS`.
- **No linux-arm64 release yet** — it builds, but the V8 snapshot forbids
  cross-compiling, so it needs a native ARM builder.
