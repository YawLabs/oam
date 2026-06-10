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

Both are self-verifying (non-zero exit if any act misbehaves), run against a
temp copy of `project/`, keep all daemon/cache state inside that temp dir,
and clean up after themselves. They use `target/release/oam` by default
(debug fallback; release recommended — debug-build process startup swamps
the daemon-cache timings in act 3).

## The four acts (human edition)

1. **The typed loop** — `oam run main.ts`: executes immediately (wrong
   answer and all), then the concurrent type check lands `OAM-TS2322` with
   file:line as a trailer. Execution never waited for the checker.
2. **The CI gate** — `oam run --check=block`: same bug, never executes.
3. **The daemon** — two `oam check` runs: the second is served from the
   fingerprint cache by the per-project daemon (`oam daemon status` shows
   `cache_hits`).
4. **Machine mode** — the same diagnostic as ODIF JSONL: stable code, span,
   docs URL. What agents consume.

## The agent loop (agent edition)

`agent-loop.mjs` speaks MCP to `oam mcp` and closes the loop a coding agent
runs all day: `oam_run` (exit 0, wrong output, plus a `typecheck`-origin
ODIF diagnostic) -> `oam_explain` -> apply a fix at the diagnostic's exact
span -> `oam_run` again -> exit 0, `total: 14`, zero diagnostics. The only
canned intelligence is the one-line repair itself; everything the "agent"
knew about the failure came from structured diagnostics. Replace the regex
with an LLM and you have the production loop.

## Notes

- `project/oam-globals.d.ts` carries ambient types for the `oam` global —
  this file becomes the published types package with M2's npm work. (The
  demo itself surfaced that gap: without it, tsgo rightly reported
  TS2304 for `oam.readTextFile`.)
- tsconfig uses `paths` aliases and JSONC comments on purpose: the loader
  and tsgo resolve them identically.
