// util.getSystemErrorMap() is the libuv errno decode table: ~85 entries whose
// NUMBERS are platform-specific (UV_ENOENT is -4058 on Windows, -2 on Unix)
// while the code/message strings come from libuv's UV_ERRNO_MAP. The
// differential pins the WHOLE table on each platform -- a missing entry or a
// Linux-valued number on Windows shows up as a raw JSON diff -- plus the two
// lookup helpers that read it and their unknown-number fallback.
import util from "node:util";

const entries = [...util.getSystemErrorMap()].sort((a, b) => a[0] - b[0]);
console.log("size", entries.length);
console.log(JSON.stringify(entries));

// getSystemErrorName / getSystemErrorMessage must agree with the map. The
// numbers come FROM the map (looked up by code) so the case stays portable
// while still printing this platform's actual values.
const byCode = new Map(entries.map(([num, [code]]) => [code, num]));
for (const code of [
  "ENOENT",
  "EBADF",
  "EACCES",
  "EEXIST",
  "EPIPE",
  "ECONNRESET",
  "EOF",
  "UNKNOWN",
  "EAI_AGAIN",
]) {
  if (!byCode.has(code)) {
    console.log(`${code} MISSING-FROM-MAP`);
    continue;
  }
  const num = byCode.get(code);
  const name = util.getSystemErrorName(num);
  const msg = util.getSystemErrorMessage(num);
  console.log(`${code} num=${num} name=${name} msg=${JSON.stringify(msg)}`);
}

// Index-derived sample over the ends and the middle: both helpers must return
// exactly what the map holds for that key.
for (const i of [0, 1, Math.floor(entries.length / 2), entries.length - 2, entries.length - 1]) {
  const [num, [code, msg]] = entries[i];
  console.log(
    `idx${i} ${num} name-match=${util.getSystemErrorName(num) === code} ` +
      `msg-match=${util.getSystemErrorMessage(num) === msg}`,
  );
}

// A valid-but-untabled negative number falls back to "Unknown system error N"
// from BOTH helpers (not undefined, not a throw).
console.log("miss-name", JSON.stringify(util.getSystemErrorName(-999999)));
console.log("miss-msg", JSON.stringify(util.getSystemErrorMessage(-999999)));

// The map is a fresh copy per call -- mutating it must not poison the runtime's
// own table.
const copy = util.getSystemErrorMap();
copy.clear();
console.log("fresh-copy", util.getSystemErrorMap().size === entries.length);

// ...and fresh ENTRIES, not just a fresh Map. node builds new [code, message]
// arrays per call, so writing through one cannot reach the lookup helpers. A
// runtime that only copies the Map passes the clear() check above and still
// lets `map.get(k)[0] = x` rewrite getSystemErrorName for the whole process --
// which is exactly what oam did until the entries were copied too.
const probeKey = entries[0][0];
const beforeMutation = util.getSystemErrorName(probeKey);
util.getSystemErrorMap().get(probeKey)[0] = "MUTATED";
console.log(
  "entry-isolated",
  util.getSystemErrorName(probeKey) === beforeMutation,
);
