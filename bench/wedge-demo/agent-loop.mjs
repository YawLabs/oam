// The agent loop, scripted: run -> read ODIF -> fix from the span ->
// re-run -> green, entirely over `oam mcp`. The ONLY canned intelligence
// here is the one-line repair (a regex an LLM wouldn't need) -- everything
// the "agent" knows about the failure came from structured diagnostics:
// stable code, file, line. No prose was scraped in the making of this loop.
//
// Self-verifying: exits non-zero unless the loop converges to exit 0 with
// the CORRECT program output. Works on a temp copy; the repo stays pristine.
//
// Usage: node agent-loop.mjs [path-to-oam-binary]
import { spawn } from "node:child_process";
import { cpSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const oamBin =
  process.argv[2] ??
  process.env.OAM_BIN ??
  join(here, "..", "..", "target", "release", process.platform === "win32" ? "oam.exe" : "oam");

const work = mkdtempSync(join(tmpdir(), "oam-wedge-"));
cpSync(join(here, "project"), work, { recursive: true });
const entry = join(work, "main.ts");

// --- minimal MCP stdio client --------------------------------------------
const mcp = spawn(oamBin, ["mcp"], {
  stdio: ["pipe", "pipe", "inherit"],
  cwd: work,
  // Keep daemons/build-info under the temp dir and short-lived: the demo
  // leaves no residue.
  env: {
    ...process.env,
    OAM_CACHE_DIR: join(work, ".oam-cache"),
    OAM_DAEMON_IDLE_MS: "45000",
  },
});
mcp.on("error", (e) => {
  console.error(`oam mcp failed to spawn: ${e.message}`);
  process.exit(1);
});
mcp.on("exit", (code) => {
  if (code !== 0 && code !== null) {
    console.error(`oam mcp exited early: ${code}`);
    process.exit(1);
  }
});
// Watchdog: the whole loop is a seconds-scale operation.
const watchdog = setTimeout(() => {
  console.error("WATCHDOG: loop did not converge in 120s");
  mcp.kill();
  process.exit(1);
}, 120_000);
watchdog.unref?.();

let buf = "";
const queue = [];
mcp.stdout.on("data", (d) => {
  buf += d.toString();
  let i;
  while ((i = buf.indexOf("\n")) >= 0) {
    const line = buf.slice(0, i).trim();
    buf = buf.slice(i + 1);
    if (line) queue.shift()?.(JSON.parse(line));
  }
});
let id = 0;
function call(method, params) {
  return new Promise((resolve) => {
    queue.push(resolve);
    mcp.stdin.write(JSON.stringify({ jsonrpc: "2.0", id: ++id, method, params }) + "\n");
  });
}
const toolText = async (name, args) => {
  const res = await call("tools/call", { name, arguments: args });
  return res.result.content[0].text;
};
// oam_run returns a JSON payload; oam_explain returns prose-for-agents.
const toolJson = async (name, args) => JSON.parse(await toolText(name, args));

// --- the loop --------------------------------------------------------------
console.log("agent> oam_run main.ts");
let run = await toolJson("oam_run", { file: entry });
console.log(`  exit=${run.exitCode} stdout=${JSON.stringify(run.stdout.trim())}`);
const typeErrors = run.diagnostics.filter((d) => d.origin === "typecheck");
if (typeErrors.length === 0) throw new Error("expected a type error in the broken project");
const diag = typeErrors[0];
console.log(`  ODIF: ${diag.code} at ${diag.spans[0].file}:${diag.spans[0].start.line} -- ${diag.message}`);

console.log(`agent> oam_explain ${diag.code}`);
const explain = await toolText("oam_explain", { code: diag.code });
console.log(`  ${explain.split("\n")[0].slice(0, 110)}...`);

// The repair: ODIF told us file + line; the edit itself is the one canned
// step (string literal in a number slot -> drop the quotes).
const file = diag.spans[0].file;
const line = diag.spans[0].start.line;
const lines = readFileSync(file, "utf8").split("\n");
const before = lines[line - 1];
lines[line - 1] = before.replace(/:\s*number\s*=\s*"(\d+)"/, ": number = $1");
if (lines[line - 1] === before) throw new Error(`repair did not apply to: ${before}`);
writeFileSync(file, lines.join("\n"));
console.log(`agent> fix ${file.split(/[\\/]/).pop()}:${line}`);
console.log(`  - ${before.trim()}`);
console.log(`  + ${lines[line - 1].trim()}`);

console.log("agent> oam_run main.ts (after fix)");
run = await toolJson("oam_run", { file: entry });
console.log(`  exit=${run.exitCode} stdout=${JSON.stringify(run.stdout.trim())}`);

mcp.stdin.end();

// --- verdict ----------------------------------------------------------------
const remaining = run.diagnostics.filter((d) => d.origin === "typecheck").length;
const correct = run.exitCode === 0 && run.stdout.includes("total: 14") && remaining === 0;
console.log(correct ? "\nLOOP CONVERGED: broken -> ODIF -> fix -> green (total: 14)" : "\nLOOP FAILED");
process.exit(correct ? 0 : 1);
