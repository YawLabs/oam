import tls from 'node:tls';
// Per-path arrival counts: a request read into the close the server's
// end() is sending, then resent from a fresh connection, counts twice.
const counts = new Map();
const bump = (p) => counts.set(p, (counts.get(p) || 0) + 1);
const server = tls.createServer({ cert: `-----BEGIN CERTIFICATE-----
MIIDRzCCAi+gAwIBAgIUXMdiPT0RoKd1ynyNQq5kRcwrF9UwDQYJKoZIhvcNAQEL
BQAwFjEUMBIGA1UEAwwLb2FtIHRlc3QgQ0EwIBcNMjYwOTE0MTExMTI1WhgPMjEy
NjA4MjExMTExMjVaMBQxEjAQBgNVBAMMCWxvY2FsaG9zdDCCASIwDQYJKoZIhvcN
AQEBBQADggEPADCCAQoCggEBALtUgW8legRgDaIObCQ75gb63jPvvGLkgrmfvL+z
zuIpFr6McD3Em6aX0fje4x8SjVF10F1HTa8pLDy4G6T/UiBuATovjMsEIqk1MLW2
F6/KfQLO35pVC6PeUCYW8UkqymVifxsPQuzdV+Hbp9VDaamHtCFhJN0sl0TAbc37
xp4WZwI1HTSQ4q+ReLSslNQiK+bwJQeKdiL7u6jzXqkb0uTxOJ2bSS2BhpPbPiNR
fZObJiFr6wtURUvy0AY9AmbNJwuWkuM0aJlOibaVIPPgVGDtZJCd8gQEdV4pKIMZ
avTN3AbNeIMmn3nZehk5jvEHxL+tjTXG8no5f5X2KFlMwi0CAwEAAaOBjDCBiTAa
BgNVHREEEzARgglsb2NhbGhvc3SHBH8AAAEwCQYDVR0TBAIwADALBgNVHQ8EBAMC
BaAwEwYDVR0lBAwwCgYIKwYBBQUHAwEwHQYDVR0OBBYEFJpXOwzKMtLLnbaIViTA
QsTBV5+8MB8GA1UdIwQYMBaAFCpmpIo81hO9R0HsjQMGk56vf8cjMA0GCSqGSIb3
DQEBCwUAA4IBAQCsP5gsrw1RHvEN9oBR1Pf+CXylfpH7It7ZMWDFW73rdhuC3Zxr
22zgG04mRt2Gd4Ufq4FCjqELVoecWx5U/hv2v/4KmVqegJkcTnMOmQ3Bs391XXa9
C+07yxnaDXE19agNm4ZACwmdf30LPaSqeVp3Y3aw8lH+5KeWrrVBpi7m8NMyHThC
Yn0a/DcxRET01zHZb6AEve5eJT6Lm0YF/DF6r4+YfGehLX892VDoWgrNCz7DpDuC
1ALfON7I9FSAJGh3iBvTbX9R7xVuKd8Za2f8Xwr/t7jK/zYxLAT9oyTH1FXIFAnP
H5shelNOFfKjeO2TTJ9u7hMSzF9fWd4EOB7u
-----END CERTIFICATE-----`, key: `-----BEGIN PRIVATE KEY-----
MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQC7VIFvJXoEYA2i
DmwkO+YG+t4z77xi5IK5n7y/s87iKRa+jHA9xJuml9H43uMfEo1RddBdR02vKSw8
uBuk/1IgbgE6L4zLBCKpNTC1thevyn0Czt+aVQuj3lAmFvFJKsplYn8bD0Ls3Vfh
26fVQ2mph7QhYSTdLJdEwG3N+8aeFmcCNR00kOKvkXi0rJTUIivm8CUHinYi+7uo
816pG9Lk8Tidm0ktgYaT2z4jUX2TmyYha+sLVEVL8tAGPQJmzScLlpLjNGiZTom2
lSDz4FRg7WSQnfIEBHVeKSiDGWr0zdwGzXiDJp952XoZOY7xB8S/rY01xvJ6OX+V
9ihZTMItAgMBAAECggEADSEodpMjMNilRqJ0JJCo1/xlQ9vy/DYVONAKo/UE9Fz6
Nx4TZSuOgpKe04Prr0CBnx/+xqA6FaNxHPWvxP9le4MPmvW84c3HECJQ6QDQ5YVF
AG63b/2zSdJJvncFL6JMJTxODvt22VskzwkHg68B4jFHXWo4Rzgvh1C6tsvavoxS
DA/J/Pl+saC6iccDtLp4lbJaMzCGGRDPjb13hqBcHoPEjF5JtN9I1bCVUZn/QFbY
7PHRptS2SDuAcoPiC8SlqZff7PSMakZzBT7Ng7kSdW3mFapJkN2NM5IsmIlTyG83
1GfTXCH1o00HXpoJ8N5YundxoG5FWlCIgcrEO1jNEQKBgQDbCJapOXVz6ULoTevi
SzdQH38UH1Ckd1rp0QYxm/MWXXyupWnBBt2iBekbFygT2bRwhtIA33CEusYmh4sN
nal6ERh5wbYYzngPaO0sHX4QVzBYleu344/pkgZxCEpG8G5oxjggj/ds15TsgLVS
KEsvXnodKmVsvDqfDFWD+ZnEzwKBgQDa8ijtlbO2ro9HgvqAr88kS/8nVlnZdXE5
9YT/DEYVsLQzduIze9G4uzI/dgSn8UtUatCvgREFB2CkQUvSeUEF4LIH6zhiI2eu
yJzhAR3tU6hXWsJSLSMildlv6ooWngdNmQg9pXTNbUjJ4dtfn1Rip5A/FKzsLxDG
/mjx6R3AQwKBgHTfA0zuXM5pY4sCsN+BVNVKyQranrPy/66NGqnz1WRUo8eoeWJG
oJHoZ3ZOB9N3sYDtXzaaAra/1iUO49JzEtAQOSgWhWx9FrDaQtrsLazYaPKLpEft
g4eUpB1B2Cg7+B2tzpsJVnNcIJmFH7rjxyJSXgQb8Bxx3zGoaiTOVQ8fAoGAST2G
iWtxkaO1FEPxTkkBbu/pK5yMM91AghXqZnMRosHYlfqn0ncSAczFE0uEZTWncFbG
9l6jdd4w6uFY3tBm+vNeOp3p35JeZa6AJBh+jVxVzNr0dA7bWP9tnC2GAejdIo0V
n6GQgAOVvMrL2qHu1Y2eCCv/aIaaAycprfrAVAcCgYBF6Hs47CZ4RPMzUnlMV8F+
F7McNeFuVRqpneXVSNB7UDuID2ttb7RTchZaG2hc84LWRV0/yjElLrG6yPxyGFIq
hnNgLVJt6pGXwWKx6CgqUvijJFPNwDhZRYtLfyCWXHDQ4E9T3C5DO7T+8lafH6NO
lAvLJ1NDDacIcdciXw6fZg==
-----END PRIVATE KEY-----` }, (sock) => {
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
console.log("redir_doubles=" + doubles((p) => p.startsWith("/redir/")));
console.log("final_max=" + maxArrivals((p) => p.startsWith('/final/')));
console.log('final_doubles=' + doubles((p) => p.startsWith('/final/')));
console.log('fin_ok=' + finOk);
console.log('fin_max=' + maxArrivals((p) => p.startsWith('/fin/')));
process.exit(0);
