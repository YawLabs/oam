// io_uring A/B sweep (Linux). Run under oam twice -- OAM_IO_URING unset
// (baseline: tokio::fs blocking pool) vs =1 (io_uring fast path) -- and compare
// cell-by-cell across file SIZE x CONCURRENCY. See docs/design/io_uring.md.
//
// Sweeping both axes is what informs the default-on decision: io_uring's win is
// concurrent in-flight ops, so the critical question is whether it still helps
// (or at least doesn't hurt) at concurrency=1 / small files, where the dispatch
// hop has no parallelism to amortize. Reads only -- the primary, clearest win
// (writes measured separately at ~1.1x).
//
// Uses the ASYNC fs API (io_uring is wired into async fs_read_file, not
// readFileSync). Prints one `mode=... size=... conc=... read_ms=...` line/cell.

import { readFile } from 'node:fs/promises';
import { mkdtempSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const SIZES = [4096, 65536, 1048576]; // 4 KiB, 64 KiB, 1 MiB
const CONCURRENCIES = [1, 8, 64];
const TARGET_BYTES = 32 * 1024 * 1024; // ~work budget per cell -> adaptive iters
const WARMUP = 3;

const mode = process.env.OAM_IO_URING ? 'io_uring' : 'baseline';

function median(xs) {
  xs.sort((a, b) => a - b);
  return xs[Math.floor(xs.length / 2)];
}

for (const size of SIZES) {
  const payload = Buffer.alloc(size, 7);
  for (const conc of CONCURRENCIES) {
    const dir = mkdtempSync(join(tmpdir(), 'oam-uring-sweep-'));
    const files = [];
    for (let i = 0; i < conc; i++) {
      const p = join(dir, `f${i}.bin`);
      writeFileSync(p, payload); // sync setup; not measured, not on the io_uring path
      files.push(p);
    }
    // Bound per-cell work to ~TARGET_BYTES so the 1 MiB x 64 cell stays cheap.
    const iters = Math.max(10, Math.min(200, Math.round(TARGET_BYTES / (size * conc))));
    const run = () => Promise.all(files.map((f) => readFile(f)));

    for (let w = 0; w < WARMUP; w++) await run();
    const times = [];
    for (let i = 0; i < iters; i++) {
      const t0 = performance.now();
      await run();
      times.push(performance.now() - t0);
    }
    rmSync(dir, { recursive: true, force: true });

    console.log(
      `mode=${mode} size=${size} conc=${conc} iters=${iters} read_ms=${median(times).toFixed(4)}`,
    );
  }
}
