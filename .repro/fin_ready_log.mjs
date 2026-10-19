import tls from 'node:tls';
import fs from 'node:fs';
const LOG = process.env.ARRIVALS || './arrivals.log';
const cert = fs.readFileSync('./leaf.pem', 'utf8');
const key = fs.readFileSync('./leaf.key', 'utf8');
const counts = new Map();
const bump = (p) => counts.set(p, (counts.get(p) || 0) + 1);
let connSeq = 0;
const server = tls.createServer({ cert, key }, (sock) => {
  const conn = ++connSeq;
  const rport = sock.remotePort;
  let buf = '';
  sock.on('data', (d) => {
    buf += d.toString('latin1');
    let i;
    while ((i = buf.indexOf('\r\n\r\n')) !== -1) {
      const head = buf.slice(0, i);
      const path = head.split(' ')[1].split('?')[0];
      const m = head.match(/content-length:\s*(\d+)/i);
      const cl = m ? Number(m[1]) : 0;
      buf = buf.slice(i + 4);
      if (buf.length >= cl) buf = buf.slice(cl);
      if (buf.length > 64 * 1024) { sock.destroy(); return; }
      bump(path);
      fs.appendFileSync(LOG, `ARR ${path} conn=${conn} rport=${rport}\n`);
      if (path.startsWith('/redir')) {
        fs.appendFileSync(LOG, `END conn=${conn} rport=${rport} path=${path}\n`);
        sock.write(`HTTP/1.1 302 Found\r\nlocation: ${path.replace('/redir', '/final')}\r\ncontent-length: 0\r\n\r\n`);
        sock.end();
      } else if (path.startsWith('/fin/')) {
        fs.appendFileSync(LOG, `END conn=${conn} rport=${rport} path=${path}\n`);
        sock.write('HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok');
        sock.end();
      } else {
        sock.write('HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok');
      }
    }
  });
  sock.on('error', () => {});
});
await new Promise((r) => server.listen(0, '127.0.0.1', r));
const base = `https://127.0.0.1:${server.address().port}`;
let getOk = 0;
await Promise.all(Array.from({ length: 8 }, async (_, c) => {
  for (let i = 0; i < 125; i++) {
    try {
      const r = await fetch(`${base}/redir/${c}_${i}?s=302`);
      if (r.status === 200 && (await r.text()) === 'ok') getOk++;
    } catch {}
  }
}));
let finOk = 0;
await Promise.all(Array.from({ length: 8 }, async (_, c) => {
  for (let i = 0; i < 50; i++) {
    try {
      const r = await fetch(`${base}/fin/${c}_${i}`, { method: 'POST', body: 'x' });
      if (r.status === 200 && (await r.text()) === 'ok') finOk++;
    } catch {}
  }
}));
await new Promise((r) => setTimeout(r, 100));
const maxArrivals = (pred) => { let m = 0; for (const [p, n] of counts) if (pred(p)) m = Math.max(m, n); return m; };
const doubles = (pred) => { let n = 0; for (const [p, c] of counts) if (pred(p) && c > 1) n++; return n; };
console.log('get_ok=' + getOk);
console.log('redir_max=' + maxArrivals((p) => p.startsWith('/redir/')));
console.log('final_max=' + maxArrivals((p) => p.startsWith('/final/')));
console.log('final_doubles=' + doubles((p) => p.startsWith('/final/')));
console.log('fin_ok=' + finOk);
console.log('fin_max=' + maxArrivals((p) => p.startsWith('/fin/')));
process.exit(0);
