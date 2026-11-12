// The agent loop, scripted: run -> check -> read ODIF -> fix from the span ->
// re-check -> re-run -> green, entirely over `oam mcp`, after the same
// initialize handshake a real MCP client performs. `oam_run` executes without
// type-checking (as `oam run` does by default), so the type diagnostic comes
// from `oam_check`. The ONLY canned intelligence
// here is the one-line repair (a regex an LLM wouldn't need) -- everything
// the "agent" knows about the failure came from structured diagnostics:
// stable code, file, line. No prose was scraped in the making of this loop.
//
// Self-verifying: exits non-zero unless the loop converges to exit 0 with
// the CORRECT program output. Works on a temp copy; the repo stays pristine,
// and the temp copy (with the daemon it started) is removed on exit.
//
// Usage: node agent-loop.mjs [path-to-oam-binary]
import { spawn, spawnSync } from "node:child_process";
import { cpSync, existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, isAbsolute, join, resolve as resolvePath } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
// Same order as demo.sh / demo.ps1: an explicit binary, then the release
// build, then the debug build, then whatever `oam` is on PATH.
const exeName = process.platform === "win32" ? "oam.exe" : "oam";
const built = ["release", "debug"]
  .map((profile) => join(here, "..", "..", "target", profile, exeName))
  .find((candidate) => existsSync(candidate));
const oamBin = process.argv[2] ?? process.env.OAM_BIN ?? built ?? "oam";

const work = mkdtempSync(join(tmpdir(), "oam-wedge-"));
cpSync(join(here, "project"), work, { recursive: true });
const entry = join(work, "main.ts");

// Keep daemon and cache state under the temp dir, so removing it removes
// everything the demo created.
const env = {
  ...process.env,
  OAM_CACHE_DIR: join(work, ".oam-cache"),
  OAM_DAEMON_IDLE_MS: "45000",
};

// --- minimal MCP stdio client --------------------------------------------
const mcp = spawn(oamBin, ["mcp"], {
  stdio: ["pipe", "pipe", "inherit"],
  cwd: work,
  env,
});

// Runs on every way out (verdict, thrown error, watchdog): stop the server
// and the type-check daemon `oam_check` started, then remove the temp copy.
// Synchronous on purpose -- 'exit' handlers cannot wait on anything.
process.on("exit", () => {
  try { mcp.kill(); } catch {}
  try { spawnSync(oamBin, ["daemon", "stop", work], { env, stdio: "ignore", timeout: 10_000 }); } catch {}
  try {
    // Retries: on Windows the directory stays busy until both processes
    // above have finished exiting.
    rmSync(work, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 });
  } catch (e) {
    console.error(`could not remove ${work}: ${e.message}`);
  }
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

// Replies are matched to requests by JSON-RPC id, never by arrival order.
let buf = "";
const pending = new Map();
mcp.stdout.on("data", (d) => {
  buf += d.toString();
  let i;
  while ((i = buf.indexOf("\n")) >= 0) {
    const line = buf.slice(0, i).trim();
    buf = buf.slice(i + 1);
    if (!line) continue;
    let msg;
    try {
      msg = JSON.parse(line);
    } catch {
      console.error(`oam mcp wrote a non-JSON line (ignored): ${line.slice(0, 120)}`);
      continue;
    }
    const waiter = pending.get(msg?.id);
    if (!waiter) continue; // a notification, or a reply to nothing we sent
    pending.delete(msg.id);
    waiter(msg);
  }
});
let id = 0;
function call(method, params) {
  return new Promise((resolve, reject) => {
    const requestId = ++id;
    pending.set(requestId, (msg) => {
      if (msg.error) reject(new Error(`${method} failed: ${msg.error.message} (${msg.error.code})`));
      else resolve(msg.result);
    });
    mcp.stdin.write(JSON.stringify({ jsonrpc: "2.0", id: requestId, method, params }) + "\n");
  });
}
const notify = (method) => mcp.stdin.write(JSON.stringify({ jsonrpc: "2.0", method }) + "\n");
const toolText = async (name, args) => {
  const result = await call("tools/call", { name, arguments: args });
  const text = result.content[0].text;
  if (result.isError) throw new Error(`${name} reported an error: ${text}`);
  return text;
};
// oam_run and oam_check return a JSON payload; oam_explain returns
// prose-for-agents.
const toolJson = async (name, args) => JSON.parse(await toolText(name, args));
const typeErrorsOf = (payload) => payload.diagnostics.filter((d) => d.origin === "typecheck");

// --- the handshake -----------------------------------------------------------
const init = await call("initialize", {
  protocolVersion: "2025-06-18",
  capabilities: {},
  clientInfo: { name: "oam-wedge-demo", version: "0" },
});
notify("notifications/initialized");
console.log(`agent> initialize: ${init.serverInfo.name} ${init.serverInfo.version}, MCP ${init.protocolVersion}`);

// --- the loop --------------------------------------------------------------
console.log("agent> oam_run main.ts");
let run = await toolJson("oam_run", { file: entry });
console.log(`  exit=${run.exitCode} stdout=${JSON.stringify(run.stdout.trim())}`);
if (run.exitCode !== 0 || !run.stdout.includes("total: 410")) {
  throw new Error("expected the broken project to run to exit 0 with the wrong answer (total: 410)");
}

console.log("agent> oam_check");
let check = await toolJson("oam_check", { path: work });
const typeErrors = typeErrorsOf(check);
if (typeErrors.length === 0) throw new Error("expected a type error in the broken project");
const diag = typeErrors[0];
console.log(`  ODIF: ${diag.code} at ${diag.spans[0].file}:${diag.spans[0].start.line} -- ${diag.message}`);

console.log(`agent> oam_explain ${diag.code}`);
const explain = await toolText("oam_explain", { code: diag.code });
console.log(`  ${explain.split("\n")[0].slice(0, 110)}...`);

// The repair: ODIF told us file + line; the edit itself is the one canned
// step (string literal in a number slot -> drop the quotes).
const spanFile = diag.spans[0].file;
const file = isAbsolute(spanFile) ? spanFile : resolvePath(work, spanFile);
const line = diag.spans[0].start.line;
const lines = readFileSync(file, "utf8").split("\n");
const before = lines[line - 1];
lines[line - 1] = before.replace(/:\s*number\s*=\s*"(\d+)"/, ": number = $1");
if (lines[line - 1] === before) throw new Error(`repair did not apply to: ${before}`);
writeFileSync(file, lines.join("\n"));
console.log(`agent> fix ${file.split(/[\\/]/).pop()}:${line}`);
console.log(`  - ${before.trim()}`);
console.log(`  + ${lines[line - 1].trim()}`);

console.log("agent> oam_check (after fix)");
check = await toolJson("oam_check", { path: work });
const remaining = typeErrorsOf(check).length;
console.log(`  type errors: ${remaining}`);

console.log("agent> oam_run main.ts (after fix)");
run = await toolJson("oam_run", { file: entry });
console.log(`  exit=${run.exitCode} stdout=${JSON.stringify(run.stdout.trim())}`);

mcp.stdin.end();

// --- verdict ----------------------------------------------------------------
const correct = run.exitCode === 0 && run.stdout.includes("total: 14") && remaining === 0;
console.log(correct ? "\nLOOP CONVERGED: broken -> ODIF -> fix -> green (total: 14)" : "\nLOOP FAILED");
// exitCode, not process.exit(): let the server see its stdin close and leave
// on its own before the 'exit' cleanup removes its working directory.
process.exitCode = correct ? 0 : 1;
