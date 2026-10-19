import tls from 'node:tls';
import fs from 'node:fs';
// Node raw TLS client: send N GETs on fresh connections, log the time between
// the response head/body and the EOF (close) the server sends after it.
const ca = fs.readFileSync('./ca.pem', 'utf8');
const port = Number(process.env.PORT);
const N = Number(process.env.N || 20);
const one = () => new Promise((resolve) => {
  const s = tls.connect({ host: '127.0.0.1', port, ca, rejectUnauthorized: true }, () => {
    s.write('GET /x HTTP/1.1\r\nhost: x\r\n\r\n');
  });
  let tResp = 0;
  s.on('data', () => { if (!tResp) tResp = process.hrtime.bigint(); });
  s.on('end', () => {
    const tEof = process.hrtime.bigint();
    s.destroy();
    resolve(Number(tEof - tResp) / 1e6);
  });
  s.on('error', () => resolve(-1));
});
const deltas = [];
for (let i = 0; i < N; i++) deltas.push(await one());
console.log('resp_to_eof_ms=' + deltas.map((d) => d.toFixed(2)).join(','));
process.exit(0);
