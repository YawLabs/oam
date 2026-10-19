import fs from 'node:fs';
const ca = fs.readFileSync('./ca.pem', 'utf8');
process.env.NODE_EXTRA_CA_CERTS = './ca.pem';
const base = `https://127.0.0.1:${process.env.PORT}`;
let getOk = 0, finOk = 0;
await Promise.all(Array.from({ length: 8 }, async (_, c) => {
  for (let i = 0; i < 125; i++) {
    try {
      const r = await fetch(`${base}/redir/${c}_${i}?s=302`);
      if (r.status === 200 && (await r.text()) === 'ok') getOk++;
    } catch {}
  }
}));
await Promise.all(Array.from({ length: 8 }, async (_, c) => {
  for (let i = 0; i < 50; i++) {
    try {
      const r = await fetch(`${base}/fin/${c}_${i}`, { method: 'POST', body: 'x' });
      if (r.status === 200 && (await r.text()) === 'ok') finOk++;
    } catch {}
  }
}));
await new Promise((r) => setTimeout(r, 200));
console.log('get_ok=' + getOk);
console.log('fin_ok=' + finOk);
console.log('CLIENT-DONE');
