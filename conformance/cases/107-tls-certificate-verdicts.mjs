// What tls.connect reports about a server certificate it accepts or refuses,
// run against live node on the same host: the error's code, message and own
// keys (no syscall, no errno), ERR_TLS_CERT_ALTNAME_INVALID's reason and
// host, and the socket's `authorized` / `authorizationError` -- with
// rejectUnauthorized:false too, where the verdict is recorded but the
// connection still opens.
//
// Regression guard (#136). oam refused every certificate rustls could not
// chain to a Mozilla root as `EIO: invalid peer certificate: UnknownIssuer`,
// a self-signed CA:TRUE certificate passed as `ca` included (rustls will not
// use a CA as a leaf; OpenSSL trusts a leaf that is itself in the store), and
// reported `authorized` as the value of rejectUnauthorized rather than the
// verifier's verdict.
//
// Fixtures: a throwaway CA (valid to 2126), the localhost leaf it signed (SAN
// DNS:localhost, IP:127.0.0.1) and the same key under an expired certificate;
// plus a self-signed CA:TRUE localhost certificate with no subjectAltName at
// all (CN=localhost only -- the shape node matches by CN).
// NODE_EXTRA_CA_CERTS needs a process environment, so it lives in the e2e
// suite; everything here runs identically under `node` and `oam run`.
import tls from "node:tls";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

const CA = `-----BEGIN CERTIFICATE-----
MIIDHzCCAgegAwIBAgIUX5ir308lg8m4hQdNnUz0UdN33DwwDQYJKoZIhvcNAQEL
BQAwFjEUMBIGA1UEAwwLb2FtIHRlc3QgQ0EwIBcNMjYwOTE0MTExMTI0WhgPMjEy
NjA4MjExMTExMjRaMBYxFDASBgNVBAMMC29hbSB0ZXN0IENBMIIBIjANBgkqhkiG
9w0BAQEFAAOCAQ8AMIIBCgKCAQEA5oXf7XNg5MHjC511VA64HF8kdBHebuI207US
fCQg9EYTe3hzOBACwsn78SNXFfmDw5E7hlF2xTuZmD3OJx9a0Ax54EoF67Z4Bigw
My6GF1oKNsmeCGn9nv62+7jm9UspForbmWE8/rC3bM37BbvS87FoogEdXQS5uNQz
4AuGbduhr27IXlScHsub4paSIrW6etllby5Ja+81NpVmwuZ32QNk+s0bwcLq8YIq
5zpemaKeTGDBbG3mIt3vYsfjg8zUTdCdkjOs8q0+BSB8OkhGpe888d5JyUxd1WiK
qiTpfG3+2Pbr0eK7pzIzeT+HDfzUInFfr7lu6lBtfjQRWSZWGwIDAQABo2MwYTAd
BgNVHQ4EFgQUKmakijzWE71HQeyNAwaTnq9/xyMwHwYDVR0jBBgwFoAUKmakijzW
E71HQeyNAwaTnq9/xyMwDwYDVR0TAQH/BAUwAwEB/zAOBgNVHQ8BAf8EBAMCAQYw
DQYJKoZIhvcNAQELBQADggEBACX/zgcVyya26/+5t6Be9duAAJs1X0VSKSzXP/Au
A+ngqWqFBPDhIzorx84d+siuRKVLOZUjObba245P4oiaJwNSz3Ihix5V3FHGZTVM
vHpVP8V7tzKpoEz89vfhueFOB0u2TVJe/099DAHrjaaza0zWa1zfxucrBAFQiQIA
2GK95UN3sSv9/rl3QlxQx8ld5QlpIjjhQL7N1JWWKcuBqDHgbfN1qwB2CWSB+v3g
YyTiYg/yyeFi173xPPS3CoiyyVyO+6ySfhwvopDJVkTdZafDpV5/d1o+AssKYX3R
Jd1U3J1YgXh3HzZEI9Yeo2jZzDogNzNObNoYdXRUoDVPMXo=
-----END CERTIFICATE-----`;

const LEAF = `-----BEGIN CERTIFICATE-----
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
-----END CERTIFICATE-----`;

const LEAF_KEY = `-----BEGIN PRIVATE KEY-----
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
-----END PRIVATE KEY-----`;

const EXPIRED = `-----BEGIN CERTIFICATE-----
MIIDRTCCAi2gAwIBAgIUXMdiPT0RoKd1ynyNQq5kRcwrF9YwDQYJKoZIhvcNAQEL
BQAwFjEUMBIGA1UEAwwLb2FtIHRlc3QgQ0EwHhcNMjAwMTAxMDAwMDAwWhcNMjEw
MTAxMDAwMDAwWjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwggEiMA0GCSqGSIb3DQEB
AQUAA4IBDwAwggEKAoIBAQC7VIFvJXoEYA2iDmwkO+YG+t4z77xi5IK5n7y/s87i
KRa+jHA9xJuml9H43uMfEo1RddBdR02vKSw8uBuk/1IgbgE6L4zLBCKpNTC1thev
yn0Czt+aVQuj3lAmFvFJKsplYn8bD0Ls3Vfh26fVQ2mph7QhYSTdLJdEwG3N+8ae
FmcCNR00kOKvkXi0rJTUIivm8CUHinYi+7uo816pG9Lk8Tidm0ktgYaT2z4jUX2T
myYha+sLVEVL8tAGPQJmzScLlpLjNGiZTom2lSDz4FRg7WSQnfIEBHVeKSiDGWr0
zdwGzXiDJp952XoZOY7xB8S/rY01xvJ6OX+V9ihZTMItAgMBAAGjgYwwgYkwGgYD
VR0RBBMwEYIJbG9jYWxob3N0hwR/AAABMAkGA1UdEwQCMAAwCwYDVR0PBAQDAgWg
MBMGA1UdJQQMMAoGCCsGAQUFBwMBMB0GA1UdDgQWBBSaVzsMyjLSy522iFYkwELE
wVefvDAfBgNVHSMEGDAWgBQqZqSKPNYTvUdB7I0DBpOer3/HIzANBgkqhkiG9w0B
AQsFAAOCAQEAPWFcRQF+fBqgAxRdsItGEcWK+wuodRNmnZ0qLd1bRTaqUASUW5ek
lNI65QaHbmCbFNgSqzkHfKT8sGs2NzHy1jvrEb2IcQrrrHzX8e19MrMCx0P0qqpr
1b19Yz9MMojQqKbIsJkh9aeQeD0ogelP571Bu0FmXHVuPb0RXyXF23z3dpjUtkAQ
QamNnNaq5R45wdYHJlNQsj9CLn7M7drDQFIsC2iakqMGHwounusQGWtzlgfmd8b+
gscDe/porMSzoYIdHpwkyK3hxLY96eO63Yhr7TzUWNCmn4P2QTiWl0Sn3OF9yTNA
z8RNjndsbTHaEcGslUIMehEVlS40ozOotw==
-----END CERTIFICATE-----`;

const SELF_SIGNED = `-----BEGIN CERTIFICATE-----
MIIDCTCCAfGgAwIBAgIUJscRiMbEzxV45KtAxD+Lly4dJrQwDQYJKoZIhvcNAQEL
BQAwFDESMBAGA1UEAwwJbG9jYWxob3N0MB4XDTI2MDYxNTEyMzAwN1oXDTI3MDYx
NTEyMzAwN1owFDESMBAGA1UEAwwJbG9jYWxob3N0MIIBIjANBgkqhkiG9w0BAQEF
AAOCAQ8AMIIBCgKCAQEAoQ5a/fh4J3VW0MPpngEpN+yRUdJtlmY6aBhV/984yEIm
ng9/MGoK0ZRdB8YYGqx4awK1z82ECwtmmdVO/77WA4q6N0CJRzmAF6BN9RgzoyKV
2w1ltowPFyB6SrVqcW1MHqA/9NX/gw/ckvcjcuazYeI857joWulUmR/iWIpSNuBJ
c6odEIkfXG9W6/GyZwlutQXnKaa8eClLqCm+hDnkPBHx+doGWxezFVeOfFAdQM8w
NXT7mj4QN3fiHFDQHI6UkSnVttu7lAAEHY978gjnVyixAPX2dY9mB/Ed4R5eSOpJ
eTR7bXH6+QmUcDJSaDblM5vB3fb3zhitEGLo/APdQQIDAQABo1MwUTAdBgNVHQ4E
FgQUPffw9cdyC1LQ2PLrzN7IZjkpKmMwHwYDVR0jBBgwFoAUPffw9cdyC1LQ2PLr
zN7IZjkpKmMwDwYDVR0TAQH/BAUwAwEB/zANBgkqhkiG9w0BAQsFAAOCAQEAWtdW
V/jSdVB5cN4GOwYXTHhh3dkYDtAPvFPCXbYacelaQe8mlRWv2BBHAhOZdmoJ3ai/
kNRw0D6pKqjcF4p17of9S07ZFCRaQGBAsDEd9jNY156AlEXu4Z8yp/kXE3fvznib
WHrQjdlDcmC2H/Ao+S7f4BkmbvsabyDbUoo+0Drk4MDvqga2azrFDdljqXQxzrEH
/mEwoi9pfukgFnFnhDE+WEqNsZQF9Yxa5QEX6d5tgbOcxS2NpKDug4xSgkpAQ0l6
XKpI59mdGTahOy9zGuNfTqVTHvrFoSXudnNHUjkfHK7Mh/VrNz9ZGpwDt5fGFD4x
E13+0jp6In545LYu+A==
-----END CERTIFICATE-----`;

const SELF_SIGNED_KEY = `-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQChDlr9+HgndVbQ
w+meASk37JFR0m2WZjpoGFX/3zjIQiaeD38wagrRlF0HxhgarHhrArXPzYQLC2aZ
1U7/vtYDiro3QIlHOYAXoE31GDOjIpXbDWW2jA8XIHpKtWpxbUweoD/01f+DD9yS
9yNy5rNh4jznuOha6VSZH+JYilI24Elzqh0QiR9cb1br8bJnCW61Becpprx4KUuo
Kb6EOeQ8EfH52gZbF7MVV458UB1AzzA1dPuaPhA3d+IcUNAcjpSRKdW227uUAAQd
j3vyCOdXKLEA9fZ1j2YH8R3hHl5I6kl5NHttcfr5CZRwMlJoNuUzm8Hd9vfOGK0Q
Yuj8A91BAgMBAAECgf9+I0AgqPlx7fSQjN/rX/1oT1+BNc2efXJBFM5GGA3gye50
3K5AvMy8V/aEoCFAwtOM/BJpLgy8mbFByk6U/mGfZIdzvpfFsMMhvetQiiPnIK89
YMDIt+kZs9YTrQIw0+lKEzgECZaUj1exwt2AoC7d+tK4qZlRmm0ngFFGBw9c6g4E
bKpZPPb62HjVAPcPPNJzj0ULTCkFQ7CPhgyz7q6UQUQJ0kM/8DWbnI9qbOWkv0qN
TafdX50piyHstcXGNFelOXmUMw1qQvbPo28qpzkxH7bDU8pShzsnySJL2HL5Wxbr
PbzZ94WOOLXfD3OmT5oW9kHDpH/zUd8pKlbYkNkCgYEA1ygCE4ZSNm0B7U8iwgBK
0Aszxpnf9f4aKKfb9CsmLaf4rxmAAqRaxT4Eki2yebqRX15Ctzlf1ryddKxNAAdh
gCcc+KAdwoJOO3pkwac+r/jsmvWqXHHi/Jn9Bhj886n5NkDGfKiCBL3rUHonwOjN
7y61ExJIy46kOM81Pm88B90CgYEAv6E6owvvAjFs1eoWD5oyUnO1HeAhKvBClDjZ
dcoY965ak5RFM4Da/HcnXAho4+pJY+4O48PIi8nQeZugm8DvpOKfivxYTI3ISDmz
CG0m7N9jJiYOPyt8dpn7Yl7R8OqFvfZAd/KkJ1wBMpsKy1MGU1pdny9mVaAVjxei
fnxNprUCgYEAn2onT6wgUe8mlFwkFrX8uHT0Ydw1EqC5ZRIqaJln6kAghCxSqqJ4
FtjCrkRpjsPrXkwLBpLeLc8GoyHe03ykgz13u8d3BV1i9bLT4KA4VE4NkSsglOpV
EnBOByyQj0GLQuVvq4F3BGhrZ+96cPaNTwC+bWkIwrnnd6gffSkRw4kCgYEAsAEE
mzZdunTs0nii9IeaipJNmnf93rM3Y23nhUEut2ZDOOLowEosV8+UrfnnZNYNvCOt
N1LeAk5FFTx0QjntoVKoWH43F3DtsDCWmDmwk8UFCsfPNAPb2A7LjekrCAxO9E+V
nNWWIbRmQTWXr3G9EJeh/5AIfMKAqqF5lJTUuTUCgYEAtzMfzgUekShhJoGov7uH
MyykhATJv+3ZlR0BCuEjgb7Lu6tu/pbgD1SkhpQ3QbM+XF5DgNJWxQATcgPWP6wy
C7rRXUYQtUTmtwTetACx3EEz7k2ixAxxdDCUPJIxGcVIPVKt6sTovr3yGLMuc4f7
I5PYIZ3kyY8EsQqX4JpTtbY=
-----END PRIVATE KEY-----`;

// The error as a client sees it: own keys in order, no syscall/errno, the
// altname error's reason and host. `cert` is not printed -- node fills it
// with the parsed peer certificate, which oam's getPeerCertificate() does
// not yet model -- only its presence and position among the keys.
function verdict(port, opts) {
  return new Promise((resolve) => {
    const s = tls.connect({ host: "127.0.0.1", port, servername: "localhost", ...opts });
    s.on("secureConnect", () => {
      resolve({ ok: true, authorized: s.authorized, authorizationError: s.authorizationError });
      s.end();
    });
    s.on("error", (e) => {
      const out = {
        ok: false,
        name: e.name,
        code: e.code,
        message: e.message,
        keys: Object.keys(e),
        hasSyscall: "syscall" in e,
        hasErrno: "errno" in e,
      };
      if ("reason" in e) out.reason = e.reason;
      if ("host" in e) out.host = e.host;
      out.authorized = s.authorized;
      out.authorizationError = s.authorizationError;
      resolve(out);
    });
  });
}

async function serve(opts) {
  const server = tls.createServer(opts, (s) => s.end("hi"));
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  return server;
}

async function report(label, server, opts) {
  console.log(label, JSON.stringify(await verdict(server.address().port, opts)));
}

// --- a self-signed CA:TRUE certificate ---------------------------------------
const selfServer = await serve({ cert: SELF_SIGNED, key: SELF_SIGNED_KEY });
await report("self-signed, no ca", selfServer, {});
await report("self-signed, as ca", selfServer, { ca: SELF_SIGNED });
await report("self-signed, in a ca array", selfServer, { ca: [CA, SELF_SIGNED] });
await report("self-signed, rejectUnauthorized:false", selfServer, { rejectUnauthorized: false });
await report("self-signed, as ca, wrong host", selfServer, { ca: SELF_SIGNED, servername: "example.com" });
// No servername: the IP the host resolves to is checked, and this
// certificate has no altnames at all, so the list it is checked against is
// empty.
await report("self-signed, as ca, by ip", selfServer, { ca: SELF_SIGNED, servername: undefined });
selfServer.close();

// --- a leaf signed by a private CA --------------------------------------------
const leafServer = await serve({ cert: LEAF, key: LEAF_KEY });
await report("private ca, no ca", leafServer, {});
await report("private ca, ca", leafServer, { ca: CA });
await report("private ca, ca as Buffer", leafServer, { ca: Buffer.from(CA) });
await report("private ca, wrong ca", leafServer, { ca: SELF_SIGNED });
await report("private ca, ca, wrong host", leafServer, { ca: CA, servername: "example.com" });
await report("private ca, ca, upper-case host", leafServer, { ca: CA, servername: "LOCALHOST" });
await report("private ca, ca, trailing dot", leafServer, { ca: CA, servername: "localhost." });
await report("private ca, ca, wrong host, rejectUnauthorized:false", leafServer, {
  ca: CA,
  servername: "example.com",
  rejectUnauthorized: false,
});
await report("private ca, no ca, wrong host", leafServer, { servername: "example.com" });
await report("private ca, rejectUnauthorized:false", leafServer, { rejectUnauthorized: false });
leafServer.close();

// --- the chain sent whole, its self-signed root included ----------------------
const chainServer = await serve({ cert: LEAF + "\n" + CA, key: LEAF_KEY });
await report("chain, no ca", chainServer, {});
await report("chain, ca", chainServer, { ca: CA });
chainServer.close();

// --- expired: the validity period beats every other complaint ---------------
const expiredServer = await serve({ cert: EXPIRED, key: LEAF_KEY });
await report("expired, ca", expiredServer, { ca: CA });
await report("expired, no ca", expiredServer, {});
await report("expired, ca, wrong host", expiredServer, { ca: CA, servername: "example.com" });
await report("expired, rejectUnauthorized:false", expiredServer, { ca: CA, rejectUnauthorized: false });
expiredServer.close();
