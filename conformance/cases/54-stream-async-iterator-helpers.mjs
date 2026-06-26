// Stream async-iterator helpers: async map with bounded concurrency (order
// preserved), an async mapper feeding a filter, and concurrency preserving
// source order despite varied per-item delays. Deterministic stdout on node
// and oam.
import { Readable } from "node:stream";

const out = [];

// async mapper, concurrency 2, output stays in source order
const mapped = await Readable.from([1, 2, 3, 4, 5])
  .map(async (x) => x * 2, { concurrency: 2 })
  .toArray();
out.push("map:" + mapped.join(","));

// async map (concurrency 3) feeding a filter
const filtered = await Readable.from([1, 2, 3, 4, 5, 6])
  .map(async (x) => x + 1, { concurrency: 3 })
  .filter((x) => x % 2 === 0)
  .toArray();
out.push("filter:" + filtered.join(","));

// concurrency must preserve SOURCE order even though later items resolve first
const ordered = await Readable.from([3, 1, 2])
  .map(async (x) => {
    await new Promise((r) => setTimeout(r, x));
    return x;
  }, { concurrency: 3 })
  .toArray();
out.push("order:" + ordered.join(","));

console.log(out.join("\n"));
