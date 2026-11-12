# The wedge demo

One small program, one classic bug, four acts. This is the M1 gate artifact:
the reproducible demonstration of what oam does that neither Node nor Bun can.

## The bug

`project/main.ts` declares `const padding: number = "10"`. After type
stripping that is perfectly runnable JavaScript — and `mean(values) + padding`
is string concatenation, so the program prints **`total: 410`** instead of
`14`. No crash. No warning. This is the class of bug that ships.

- **Node 26** runs it silently (native type stripping checks nothing).
- **Bun** runs it silently (types-blind execution).
- **oam** runs it instantly — *and tells you, and tells your agents.*

## Run it

```
# human edition (PowerShell / POSIX)
.\demo.ps1            # or: ./demo.sh
# agent edition (drives `oam mcp` end to end)
node agent-loop.mjs
```

All three are self-verifying (non-zero exit if any act misbehaves), run
against a temp copy of `project/`, keep all daemon/cache state inside that
temp dir, and on exit stop the daemon they started and remove the copy.
`demo.ps1` also restores the two environment variables it sets
(`OAM_CACHE_DIR`, `OAM_CHECK_WAIT_MS`), so the calling session is unchanged.
They take an explicit binary (first argument / `-OamBin`, or `OAM_BIN` for
the two non-PowerShell ones), else `target/release/oam`, else
`target/debug/oam`, else `oam` on PATH (release recommended — debug-build
process startup swamps the daemon-cache timings in act 3).

## The four acts (human edition)

1. **The typed loop** — `oam run --check=warn main.ts`: executes immediately (wrong
   answer and all), then the concurrent type check lands `OAM-TS2322` with
   file:line as a trailer. Execution never waited for the checker. Verified:
   exit 0, `total: 410` printed, `TS2322` reported.
2. **The CI gate** — `oam run --check=block`: same bug, never executes.
   Verified: non-zero exit.
3. **The daemon** — two `oam check` runs: the second is served from the
   fingerprint cache by the per-project daemon. Verified by the daemon's own
   counter (`oam daemon status` shows `cache_hits` >= 1); the two timings are
   printed for the reader and are not part of the verdict.
4. **Machine mode** — the same diagnostic as ODIF JSONL: stable code, span,
   docs URL. What agents consume. Verified: an `OAM-TS2322` JSON line.

## The agent loop (agent edition)

`agent-loop.mjs` speaks MCP to `oam mcp` -- the `initialize` handshake first,
replies matched to requests by id -- and closes the loop a coding agent runs
all day: `oam_run` (exit 0, wrong output: like `oam run`, the tool executes
without type-checking) -> `oam_check` (the `typecheck`-origin ODIF
diagnostic) -> `oam_explain` -> apply a fix at the diagnostic's exact span
-> `oam_check` again (zero type errors) -> `oam_run` again -> exit 0,
`total: 14`. The only
canned intelligence is the one-line repair itself; everything the "agent"
knew about the failure came from structured diagnostics. Replace the regex
with an LLM and you have the production loop.

## Notes

- The `oam` global needs no ambient types in the project any more. The demo
  used to carry `project/oam-globals.d.ts` for it (without one, tsgo rightly
  reported TS2304 for `oam.readTextFile`); oam now injects its own
  declarations -- the `oam:` modules and the global -- into every check, so
  the demo checks clean with nothing in the project but its own sources.
- tsconfig uses `paths` aliases and JSONC comments on purpose: the loader
  and tsgo resolve them identically.
