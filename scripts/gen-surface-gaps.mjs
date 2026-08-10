#!/usr/bin/env node
// Regenerate this platform's section of conformance/surface-gaps.json -- the
// ratchet of builtin export names oam does not yet provide.
//
//   node scripts/gen-surface-gaps.mjs [path/to/oam]
//   node scripts/gen-surface-gaps.mjs --allow-regression   (see below)
//
// Runs conformance/runners/surface.mjs under BOTH oam and the node on PATH,
// then writes every Node export name oam lacks. Run it after CLOSING gaps so
// the list shrinks; the conformance gate fails on a listed name that oam now
// has, which is what stops the file from silently covering a regression.
//
// PER-PLATFORM, and it MERGES. Node's own builtin surface is platform-
// conditional -- `node:constants` carries the WSA* errnos on Windows and the
// POSIX signal set elsewhere, and `node:process` only has getuid/setgid/... on
// POSIX -- so a single flat list cannot describe three platforms at once.
// Each run replaces ONLY platforms[process.platform] and leaves the others
// untouched; regenerating on Linux therefore cannot wipe the Windows baseline.
// The gate reads the section matching the host it runs on.
//
// With no argument it BUILDS oam first and probes the same debug binary the
// gate uses (xtask's ensure_oam_built). Probing a stale or different-profile
// binary writes a ratchet that does not describe the code under test -- the
// gate would then invent gaps or hide them, which is exactly the stale-binary
// trap ensure_oam_built's comment documents. An explicit path argument is
// taken as-is, on the caller's word.
//
// It never invents entries: each section is a snapshot of measured reality on
// the host that generated it.
import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync, existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const repo = join(dirname(fileURLToPath(import.meta.url)), "..");
const runner = join(repo, "conformance/runners/surface.mjs");
const gapsPath = join(repo, "conformance/surface-gaps.json");

const args = process.argv.slice(2);
const allowRegression = args.includes("--allow-regression");
let oamBin = args.find((a) => !a.startsWith("--"));
if (!oamBin) {
  // cargo's freshness check makes this a no-op when current, and the profile
  // matches the gate's default so both measure the same binary.
  console.log("building oam (debug)...");
  try {
    execFileSync("cargo", ["build", "-p", "oam_cli"], { cwd: repo, stdio: "inherit" });
  } catch (e) {
    if (e.code === "ENOENT") {
      // A non-interactive ssh does not source ~/.profile, so cargo is off
      // PATH on a remote host unless ~/.cargo/env was sourced first. The raw
      // "spawnSync cargo ENOENT" gives no hint of that.
      console.error(
        "cargo is not on PATH. On a remote build host run this through the " +
          "dispatch (`bash scripts/build-remote.sh surface-gaps`), which sources " +
          "~/.cargo/env, or source it yourself first. Alternatively pass a path " +
          "to an already-built oam as the first argument.",
      );
      process.exit(1);
    }
    throw e;
  }
  oamBin = join(repo, `target/debug/oam${process.platform === "win32" ? ".exe" : ""}`);
}

const read = (cmd, cmdArgs) =>
  JSON.parse(execFileSync(cmd, cmdArgs, { cwd: repo, encoding: "utf8", maxBuffer: 64 << 20 }));

const oam = read(oamBin, ["run", runner, "--no-check"]).exportNames;
const node = read(process.execPath, [runner]).exportNames;

const absentModules = [];
const missingExports = {};
// Export names inside modules oam does not register at all. The per-name diff
// never sees them (an absent module is recorded at module granularity), so
// they have to be counted here or they score as present -- `sys` alone is 49.
let absentModuleNames = 0;
for (const [name, nodeKeys] of Object.entries(node)) {
  if (!Array.isArray(nodeKeys)) continue;
  const ours = oam[name];
  if (ours === false || ours === null || ours === undefined) {
    absentModules.push(name);
    absentModuleNames += nodeKeys.length;
    continue;
  }
  const missing = nodeKeys.filter((k) => !ours.includes(k));
  if (missing.length) missingExports[name] = missing;
}
absentModules.sort();

const total = Object.values(node).filter(Array.isArray).reduce((a, b) => a + b.length, 0);
const missingCount = Object.values(missingExports).reduce((a, b) => a + b.length, 0);
const present = total - missingCount - absentModuleNames;

// Merge into whatever is already there; never clobber a sibling platform.
const existing = existsSync(gapsPath) ? JSON.parse(readFileSync(gapsPath, "utf8")) : {};
const platforms = existing.platforms ?? {};
const previous = platforms[process.platform];

// Recording a gap that was not there before is the one direction that weakens
// the gate -- it is how a regression gets laundered into "known debt".
//
// Compared as NAME SETS, not totals: a run that closes five gaps while
// introducing three nets negative, so a count comparison would wave the three
// through. Any newly-missing name is refused on its own merits, however much
// else improved in the same run.
if (previous && !allowRegression) {
  const had = (module) => new Set(previous.missingExports?.[module] ?? []);
  const regressions = [];
  for (const [module, names] of Object.entries(missingExports)) {
    const before = had(module);
    for (const name of names) if (!before.has(name)) regressions.push(`${module}.${name}`);
  }
  const absentBefore = new Set(previous.absentModules ?? []);
  for (const module of absentModules) {
    if (!absentBefore.has(module)) regressions.push(`${module} (whole module)`);
  }
  if (regressions.length) {
    console.error(
      `refusing to write: ${regressions.length} export(s) are newly missing on ` +
        `${process.platform} and would be recorded as known debt:\n  ` +
        `${regressions.slice(0, 20).join("\n  ")}` +
        (regressions.length > 20 ? `\n  ...and ${regressions.length - 20} more` : "") +
        "\n\nThat is the regression the ratchet exists to catch. Implement them, or " +
        "re-run with --allow-regression if the growth is genuinely expected (a Node " +
        "upgrade publishing new exports).",
    );
    process.exit(1);
  }
}

platforms[process.platform] = {
  generatedAgainst: `${process.version} on ${process.platform}-${process.arch}`,
  counts: {
    nodeExportNames: total,
    present,
    missing: missingCount,
    absentModules: absentModules.length,
    absentModuleExportNames: absentModuleNames,
  },
  absentModules,
  missingExports: Object.fromEntries(Object.entries(missingExports).sort()),
};

writeFileSync(
  gapsPath,
  `${JSON.stringify(
    {
      _comment:
        "Builtin export names oam does not provide, per platform, measured against the " +
        "node named in each section. A builtin's ESM named exports are its module " +
        "object's own enumerable keys, so every name here is a link-time SyntaxError " +
        "('does not provide an export named X') for any program that imports it -- even " +
        "one that never calls it. Bundled CLIs import the union of their dependency " +
        "tree, which is how a single unused name takes down a whole program. This list " +
        "GATES: on a platform with a section, a missing name absent from it fails " +
        "`cargo run -p xtask -- conformance`, and so does a listed name oam now has. It " +
        "may only shrink. Sections are keyed by process.platform because Node's own " +
        "surface is platform-conditional (WSA* errnos on Windows, getuid/setgid on " +
        "POSIX). Regenerate the current platform's section with " +
        "scripts/gen-surface-gaps.mjs; it merges and never touches the others.",
      platforms: Object.fromEntries(Object.entries(platforms).sort()),
    },
    null,
    2,
  )}\n`,
);

console.log(
  `surface-gaps.json [${process.platform}]: ${present}/${total} export names present, ` +
    `${missingCount} missing across ${Object.keys(missingExports).length} modules, ` +
    `${absentModules.length} modules absent (${absentModuleNames} more names)`,
);
const others = Object.keys(platforms).filter((p) => p !== process.platform);
if (others.length) console.log(`preserved sections for: ${others.join(", ")}`);
else {
  console.log(
    "no other platform sections yet -- the gate SKIPS (loudly) on a platform with no " +
      "section, so run this on each release host and commit the result.",
  );
}
