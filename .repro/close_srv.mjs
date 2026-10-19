import tls from 'node:tls';
import fs from 'node:fs';
const cert = fs.readFileSync('./leaf.pem', 'utf8');
const key = fs.readFileSync('./leaf.key', 'utf8');
const server = tls.createServer({ cert, key }, (sock) => {
  let buf = '';
  sock.on('data', (d) => {
    buf += d.toString('latin1');
    const i = buf.indexOf('\r\n\r\n');
    if (i === -1) return;
    buf = '';
    sock.write('HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok');
    sock.end();
  });
  sock.on('error', () => {});
});
await new Promise((r) => server.listen(Number(process.env.PORT), '127.0.0.1', r));
console.log('server-on');
setInterval(() => {}, 1000);
