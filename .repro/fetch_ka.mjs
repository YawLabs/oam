import https from 'node:https';
import fs from 'node:fs';
const server = https.createServer(
  { cert: fs.readFileSync('./leaf.pem', 'utf8'), key: fs.readFileSync('./leaf.key', 'utf8') },
  (req, res) => { res.writeHead(200, { 'content-length': '2' }); res.end('ok'); },
);
await new Promise((r) => server.listen(0, '127.0.0.1', r));
const base = `https://127.0.0.1:${server.address().port}`;
const get = async () => { const r = await fetch(base + '/k'); return await r.text(); };
await get();
const N = 500;
const t0 = process.hrtime.bigint();
for (let i = 0; i < N; i++) await get();
const t1 = process.hrtime.bigint();
console.log('fetch_keepalive_ms=' + Number(t1 - t0) / 1e6 + ' per_req_us=' + Number(t1 - t0) / 1e3 / N);
server.close();
process.exit(0);
