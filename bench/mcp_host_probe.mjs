// MCP-host dogfood probe: does a real @yawlabs MCP server complete an MCP
// handshake (initialize -> notifications/initialized -> tools/list) when hosted
// under oam, and how does cold start compare to node? Spawns the server's built
// dist entry under each runtime and speaks newline-delimited JSON-RPC 2.0 over
// stdio (the StdioServerTransport the @yawlabs servers use).
//
// This is the dogfood gate for "wire Yaw's MCP sidecars to run on oam": if the
// handshake succeeds under oam, the launcher can prefer `oam run` (with a node
// fallback). If it fails, the captured stderr is the oam node:-surface gap.
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
      try { child.kill(); } catch {}
      resolve({ label, ok, initMs, toolCount, err: err || null, stderr: errBuf.slice(0, 600) });
    };
    const timer = setTimeout(() => finish(false, 'timeout (8s)'), 8000);
    child.on('error', (e) => { clearTimeout(timer); finish(false, `spawn failed: ${e.message}`); });
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
          initMs = performance.now() - t0;
          child.stdin.write(JSON.stringify({ jsonrpc: '2.0', method: 'notifications/initialized' }) + '\n');
          child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id: 2, method: 'tools/list', params: {} }) + '\n');
        } else if (msg.id === 2) {
          toolCount = (msg.result && msg.result.tools && msg.result.tools.length) || 0;
          clearTimeout(timer);
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

const node = await probe('node', process.execPath, [entry]);
const oam = await probe('oam', oamBin, ['run', entry]);
for (const r of [node, oam]) {
  const line = `${r.label.padEnd(5)} ok=${r.ok} init=${r.initMs != null ? r.initMs.toFixed(1) : '-'}ms tools=${r.toolCount != null ? r.toolCount : '-'}`;
  console.log(r.err ? `${line} ERR=${r.err}` : line);
  if (!r.ok && r.stderr) console.log(`  stderr: ${r.stderr.replace(/\s+/g, ' ').trim()}`);
}
process.exit(node.ok && oam.ok ? 0 : 1);
