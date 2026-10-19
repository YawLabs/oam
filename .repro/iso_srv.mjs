import tls from 'node:tls';
import fs from 'node:fs';
const cert = fs.readFileSync('./leaf.pem', 'utf8');
const key = fs.readFileSync('./leaf.key', 'utf8');
const counts = new Map();
const bump = (p) => counts.set(p, (counts.get(p) || 0) + 1);
const server = tls.createServer({ cert, key }, (sock) => {
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
      if (path.startsWith('/redir')) {
        sock.write(`HTTP/1.1 302 Found\r\nlocation: ${path.replace('/redir', '/final')}\r\ncontent-length: 0\r\n\r\n`);
        sock.end();
      } else if (path.startsWith('/fin/')) {
        sock.write('HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok');
        sock.end();
      } else {
        sock.write('HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok');
      }
    }
  });
  sock.on('error', () => {});
});
await new Promise((r) => server.listen(Number(process.env.PORT), '127.0.0.1', r));
console.log('server-on ' + server.address().port);
const maxArrivals = (pred) => { let m = 0; for (const [p, n] of counts) if (pred(p)) m = Math.max(m, n); return m; };
const doubles = (pred) => { let n = 0; for (const [p, c] of counts) if (pred(p) && c > 1) n++; return n; };
process.on('SIGTERM', () => {
  console.log('redir_max=' + maxArrivals((p) => p.startsWith('/redir/')));
  console.log('final_max=' + maxArrivals((p) => p.startsWith('/final/')));
  console.log('final_doubles=' + doubles((p) => p.startsWith('/final/')));
  console.log('fin_max=' + maxArrivals((p) => p.startsWith('/fin/')));
  process.exit(0);
});
setInterval(() => {}, 1000);
