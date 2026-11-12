// exec / execFile / the sync spawns: maxBuffer defaults to 1 MiB (1024 * 1024)
// when the options carry no `maxBuffer` at all, and a value that is neither
// null nor undefined must be a non-negative number (ERR_OUT_OF_RANGE, thrown
// before anything runs). What an explicit null, undefined or 0 then means
// differs between the async and the sync path, and is pinned here as node has
// it rather than described.
import { exec, execFile, execFileSync, spawnSync } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const dir = path.join(os.tmpdir(), `oam-conf-maxbuf-default-${process.pid}`);
mkdirSync(dir, { recursive: true });

// Writes argv[2] bytes to stdout in 1024-byte chunks.
const noisy = path.join(dir, "noisy.cjs");
writeFileSync(
  noisy,
  "const total=Number(process.argv[2]);const chunk='x'.repeat(1024);" +
    "for(let n=0;n<total;n+=chunk.length)process.stdout.write(chunk);",
);

const MIB = 1024 * 1024;
const run = (bytes, options) =>
  new Promise((done) => {
    execFile(process.execPath, [noisy, String(bytes)], options, (err, stdout) => {
      done({ code: err ? err.code : null, length: stdout.length });
    });
  });
const report = (label, r) => console.log(label, r.code, r.length);
const sync = (label, bytes, options) => {
  const r = spawnSync(process.execPath, [noisy, String(bytes)], options);
  console.log(label, r.error ? r.error.code : null, r.error ? "" : r.stdout.length);
};
const thrown = (fn) => {
  try {
    fn();
    return "no throw";
  } catch (e) {
    return `${e.name} ${e.code}`;
  }
};

try {
  // Exactly the default: fits.
  report("default, 1 MiB:", await run(MIB, {}));
  // One chunk past it: the overflow error, and exactly maxBuffer bytes kept.
  report("default, 1 MiB + 1024:", await run(MIB + 1024, {}));
  // A larger explicit limit lets the same output through.
  report("explicit 2 MiB:", await run(MIB + 1024, { maxBuffer: 2 * MIB }));
  report("explicit Infinity:", await run(MIB + 1024, { maxBuffer: Infinity }));
  // 0 is a limit, not "unset".
  report("explicit 0:", await run(1024, { maxBuffer: 0 }));
  report("explicit null:", await run(2048, { maxBuffer: null }));
  report("explicit undefined:", await run(MIB + 1024, { maxBuffer: undefined }));

  // The sync spawns share the default, and report ENOBUFS.
  sync("spawnSync default, 1 MiB:", MIB, {});
  sync("spawnSync default, 1 MiB + 1024:", MIB + 1024, {});
  sync("spawnSync explicit 2 MiB:", MIB + 1024, { maxBuffer: 2 * MIB });
  sync("spawnSync explicit 0:", MIB + 1024, { maxBuffer: 0 });
  sync("spawnSync explicit null:", MIB + 1024, { maxBuffer: null });
  sync("spawnSync explicit undefined:", MIB + 1024, { maxBuffer: undefined });

  // Validated before a child is started.
  const noop = () => {};
  console.log("exec -1:", thrown(() => exec("exit 0", { maxBuffer: -1 }, noop)));
  console.log("execFile 'x':", thrown(() => execFile(process.execPath, ["-v"], { maxBuffer: "x" }, noop)));
  console.log("execFile NaN:", thrown(() => execFile(process.execPath, ["-v"], { maxBuffer: NaN }, noop)));
  console.log("spawnSync -1:", thrown(() => spawnSync(process.execPath, ["-v"], { maxBuffer: -1 })));
  console.log("execFileSync '1':", thrown(() => execFileSync(process.execPath, ["-v"], { maxBuffer: "1" })));
} finally {
  rmSync(dir, { recursive: true, force: true });
}
