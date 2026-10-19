import https from 'node:https';
import fs from 'node:fs';
const ca = fs.readFileSync('./ca.pem', 'utf8');
const agent = new https.Agent({ ca, keepAlive: true, maxSockets: 1 });
const server = https.createServer({ cert: fs.readFileSync('./leaf.pem', 'utf8'), key: fs.readFileSync('./leaf.key', 'utf8') }, (req, res) => {
  res.writeHead(200, { 'content-length': '2' });
  res.end('ok');
});
await new Promise((r) => server.listen(0, '127.0.0.1', r));
const port = server.address().port;
const get = () => new Promise((resolve, reject) => {
  const req = https.request({ port, host: '127.0.0.1', path: '/k', ca, agent }, (res) => {
    let s = '';
    res.on('data', (d) => (s += d));
    res.on('end', () => resolve(s));
  });
  req.on('error', reject);
  req.end();
});
// Warm the connection, then time sequential keep-alive requests.
await get();
const N = 2000;
const t0 = process.hrtime.bigint();
for (let i = 0; i < N; i++) await get();
const t1 = process.hrtime.bigint();
console.log('keepalive_seq_ms=' + Number(t1 - t0) / 1e6 + ' per_req_us=' + Number(t1 - t0) / 1e3 / N);
agent.destroy();
server.close();
process.exit(0);
