// io_uring A/B micro-benchmark (Linux). Run the SAME script under oam twice --
// once with OAM_IO_URING unset (baseline: tokio::fs blocking pool) and once with
// OAM_IO_URING=1 (the io_uring fast path) -- and compare the medians. See
// docs/design/io_uring.md.
//
// It must use the ASYNC, CONCURRENT fs APIs: io_uring is wired into the async
// fs_read_file / fs_write_file ops (NOT readFileSync/writeFileSync), and the
// win is concurrent in-flight ops, so each timed iteration does Promise.all over
// many files. Self-timing: prints one `mode=... read_ms=... write_ms=...` line.

import { readFile, writeFile } from 'node:fs/promises';
import { mkdtempSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const N_FILES = 64;
const SIZE = 16 * 1024;
const ITERS = 200;
const WARMUP = 5;

const mode = process.env.OAM_IO_URING ? 'io_uring' : 'baseline';
const dir = mkdtempSync(join(tmpdir(), 'oam-iouring-ab-'));
const payload = Buffer.alloc(SIZE, 7);

// Setup via SYNC writes (not part of the measurement; sync ops don't hit the
// io_uring path).
const files = [];
for (let i = 0; i < N_FILES; i++) {
  const p = join(dir, `f${i}.bin`);
  writeFileSync(p, payload);
  files.push(p);
}

function median(xs) {
  xs.sort((a, b) => a - b);
  return xs[Math.floor(xs.length / 2)];
}

async function timeLoop(fn) {
  for (let w = 0; w < WARMUP; w++) await fn();
  const times = [];
  for (let i = 0; i < ITERS; i++) {
    const t0 = performance.now();
    await fn();
    times.push(performance.now() - t0);
  }
  return median(times);
}

// Read phase: N concurrent async reads per iteration.
const readMs = await timeLoop(() => Promise.all(files.map((f) => readFile(f))));

// Write phase: N concurrent async (non-append) writes per iteration.
const writeMs = await timeLoop(() =>
  Promise.all(files.map((f) => writeFile(f, payload))),
);

rmSync(dir, { recursive: true, force: true });

console.log(
  `mode=${mode} read_ms=${readMs.toFixed(4)} write_ms=${writeMs.toFixed(4)} ` +
    `files=${N_FILES} size=${SIZE} iters=${ITERS}`,
);
