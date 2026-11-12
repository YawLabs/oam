// MCP-host dogfood probe: does a real @yawlabs MCP server complete an MCP
// handshake (initialize -> notifications/initialized -> tools/list) when hosted
// under oam, and how does cold start compare to node? Spawns the server's built
// dist entry under each runtime and speaks newline-delimited JSON-RPC 2.0 over
// stdio (the StdioServerTransport the @yawlabs servers use).
//
// HISTORICAL: this was the first dogfood probe for "wire Yaw's MCP sidecars to
// run on oam". Nothing in scripts/ or xtask runs it; the gate for that job is
// scripts/mcp-sidecar-matrix.mjs, which covers the same handshake with a
// control arm. Kept as a one-server, by-hand probe: if the handshake fails
// under oam, the captured stderr is the oam node:-surface gap.
//
// The init time is wall-clock from spawn and is indicative only. Each runtime
// gets one discarded run first, so neither measured run pays for a cold file
// cache on the server's files.
//
// Usage:
//   node bench/mcp_host_probe.mjs <server-dist-index.js> [oam-binary]
import { spawn } from 'node:child_process';
import { performance } from 'node:perf_hooks';

const entry = process.argv[2];
const oamBin = process.argv[3] || 'oam';
if (!entry) {
  console.error('usage: node bench/mcp_host_probe.mjs <server-dist-index.js> [oam-binary]');
  process.exit(2);
}

function probe(label, cmd, args) {
  return new Promise((resolve) => {
    const t0 = performance.now();
    const child = spawn(cmd, args, { stdio: ['pipe', 'pipe', 'pipe'] });
    let buf = '';
    let errBuf = '';
    let initMs = null;
    let toolCount = null;
    let done = false;
    const finish = (ok, err) => {
      if (done) return;
      done = true;
      clearTimeout(timer);
      try { child.kill(); } catch {}
      resolve({ label, ok, initMs, toolCount, err: err || null, stderr: errBuf.slice(0, 600) });
    };
    const timer = setTimeout(() => finish(false, 'timeout (8s)'), 8000);
    child.on('error', (e) => finish(false, `spawn failed: ${e.message}`));
    // A server that dies before answering is a crash, not a timeout: report it
    // now, with its exit status, instead of waiting out the timer. 'close'
    // rather than 'exit' so the stderr captured for the report is complete.
    child.on('close', (code, signal) => {
      finish(false, `server exited before the handshake finished (${signal ?? `code ${code}`})`);
    });
    // The server may be gone by the time a request is written.
    child.stdin.on('error', () => {});
    child.stderr.on('data', (c) => { errBuf += c.toString('utf8'); });
    child.stdout.on('data', (c) => {
      buf += c.toString('utf8');
      let i;
      while ((i = buf.indexOf('\n')) >= 0) {
        const line = buf.slice(0, i).trim();
        buf = buf.slice(i + 1);
        if (!line) continue;
        let msg;
        try { msg = JSON.parse(line); } catch { continue; }
        if (msg.id === 1 && initMs === null) {
          // An error reply to initialize is a failed handshake, not an init.
          if (msg.error || !msg.result) {
            finish(false, `initialize failed: ${JSON.stringify(msg.error ?? msg).slice(0, 200)}`);
            return;
          }
          initMs = performance.now() - t0;
          child.stdin.write(JSON.stringify({ jsonrpc: '2.0', method: 'notifications/initialized' }) + '\n');
          child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id: 2, method: 'tools/list', params: {} }) + '\n');
        } else if (msg.id === 2) {
          if (msg.error || !msg.result) {
            finish(false, `tools/list failed: ${JSON.stringify(msg.error ?? msg).slice(0, 200)}`);
            return;
          }
          toolCount = (msg.result.tools && msg.result.tools.length) || 0;
          finish(true);
        }
      }
    });
    child.stdin.write(JSON.stringify({
      jsonrpc: '2.0', id: 1, method: 'initialize',
      params: { protocolVersion: '2024-11-05', capabilities: {}, clientInfo: { name: 'oam-probe', version: '0' } },
    }) + '\n');
  });
}

// Discarded priming runs, one per runtime (see the header).
await probe('node', process.execPath, [entry]);
await probe('oam', oamBin, ['run', entry]);
const node = await probe('node', process.execPath, [entry]);
const oam = await probe('oam', oamBin, ['run', entry]);
for (const r of [node, oam]) {
  const line = `${r.label.padEnd(5)} ok=${r.ok} init=${r.initMs != null ? r.initMs.toFixed(1) : '-'}ms tools=${r.toolCount != null ? r.toolCount : '-'}`;
  console.log(r.err ? `${line} ERR=${r.err}` : line);
  if (!r.ok && r.stderr) console.log(`  stderr: ${r.stderr.replace(/\s+/g, ' ').trim()}`);
}
process.exit(node.ok && oam.ok ? 0 : 1);
