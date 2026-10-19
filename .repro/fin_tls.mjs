import tls from 'node:tls';
// Per-path arrival counts: a request read into the close the server's
// end() is sending, then resent from a fresh connection, counts twice.
const counts = new Map();
const bump = (p) => counts.set(p, (counts.get(p) || 0) + 1);
const server = tls.createServer({ cert: `__CERT__`, key: `__KEY__` }, (sock) => {
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
        // 302 and close in one callback: over TLS the close is a
        // close_notify alert and then the FIN.
        sock.write(`HTTP/1.1 302 Found\r\nlocation: ${path.replace('/redir', '/final')}\r\ncontent-length: 0\r\n\r\n`);
        sock.end();
      } else if (path.startsWith('/fin/')) {
        // 200 + keep-alive answered, then the close at once.
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

// 8 chains x 125 sequential GETs, each followed through its 302: the
// 1,000 hops #155 measured with.
let getOk = 0;
await Promise.all(Array.from({ length: 8 }, async (_, c) => {
  for (let i = 0; i < 125; i++) {
    try {
      const r = await fetch(`${base}/redir/${c}_${i}?s=302`);
      if (r.status === 200 && (await r.text()) === 'ok') getOk++;
    } catch {}
  }
}));

// 8 chains x 50 sequential POSTs against the 200-then-close shape: every
// pooled connection each chain meets is one whose close is in flight.
let finOk = 0;
await Promise.all(Array.from({ length: 8 }, async (_, c) => {
  for (let i = 0; i < 50; i++) {
    try {
      const r = await fetch(`${base}/fin/${c}_${i}`, { method: 'POST', body: 'x' });
      if (r.status === 200 && (await r.text()) === 'ok') finOk++;
    } catch {}
  }
}));

// Let a late server-side arrival land before reading the counts.
await new Promise((r) => setTimeout(r, 100));
const maxArrivals = (pred) => {
  let max = 0;
  for (const [p, n] of counts) if (pred(p)) max = Math.max(max, n);
  return max;
};
const doubles = (pred) => {
  let n = 0;
  for (const [p, c] of counts) if (pred(p) && c > 1) n++;
  return n;
};
console.log('get_ok=' + getOk);
console.log('redir_max=' + maxArrivals((p) => p.startsWith('/redir/')));
console.log('final_max=' + maxArrivals((p) => p.startsWith('/final/')));
console.log('final_doubles=' + doubles((p) => p.startsWith('/final/')));
console.log('fin_ok=' + finOk);
console.log('fin_max=' + maxArrivals((p) => p.startsWith('/fin/')));
process.exit(0);
