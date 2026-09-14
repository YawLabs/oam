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

// A leaf chained through a non-self-signed intermediate to a root the client
// is not given, and a leaf whose validity period has not started -- for the
// incomplete-chain and not-yet-valid verdicts. Minted with openssl, embedded.
const CHAIN_ROOT = `-----BEGIN CERTIFICATE-----
MIIDIzCCAgugAwIBAgIUaPSZpqSb/Y/gC+okpLcSTLrO6MMwDQYJKoZIhvcNAQEL
BQAwGDEWMBQGA1UEAwwNb2FtIHRlc3Qgcm9vdDAgFw0yNjA5MTQyMjEyMDFaGA8y
MTI2MDgyMTIyMTIwMVowGDEWMBQGA1UEAwwNb2FtIHRlc3Qgcm9vdDCCASIwDQYJ
KoZIhvcNAQEBBQADggEPADCCAQoCggEBAMmr+h+3f4TYbkQ+RlaxK4FnjkAzx7Ji
Qut16fnLV+GGdmYQswwTObN2366VrjD8zD0VoBJgU5Vfd6HNrJRfEr5Xn2C6dY5q
JGaou+6k3xUIdwq5IRDX+LAHr9CbssRjwlxkfXhQKpMWr0TK1/7UFr9BENmv1WY+
sy7vqbxcHq0ne4Pxcigmxobhs09Jw0FsbRlIn6TuxBIoxF6Fti+S5IroCbWKjphd
sy+wcwIAT8rT86jtpdZ/F1KrSCvFPfRtk0Y9CzDKxQNjDaCXr+MgqhYfUXMHBCsc
/4we9A0gfygDdYf17yB4C/7Qz6aW9Zb7E+BDsWxwTPF1+dNb5BQhNYECAwEAAaNj
MGEwHQYDVR0OBBYEFG6HviMKwbfJAAvAJMWqqPR1iFGvMB8GA1UdIwQYMBaAFG6H
viMKwbfJAAvAJMWqqPR1iFGvMA8GA1UdEwEB/wQFMAMBAf8wDgYDVR0PAQH/BAQD
AgEGMA0GCSqGSIb3DQEBCwUAA4IBAQCdZTV8e6W3D5D2yA2/dEvt5/VjmjTWR+Qu
DYLRdhPYSgWHBwfU0tVctscGMT8kaKo0nqwcFR1WaDBYucJxHzIDqoPdpyvAz4kl
kUE9gQEFpAaOKSwgWY3X1RaCOsrMJsfH5G2XmSoD2iUllAZehmri3m2dIPsy2bCC
0dZhETxuJVfozhkjjOrd18lmrJvauPElSKJodaH76FZ6NNoeu3zR6/KAvhf23lj+
Pkdn0P9TUDOjGIXPBtOE1ZcwZloDvc3HZ5FXF0d10VOI58KmVILYzwLOAOPGi8MG
WB3rTJQHuG+glT3zSYXRBQgo6S+HW+ktZ/RbLArS+SkDGiyIcXui
-----END CERTIFICATE-----`;
const CHAIN_INTER = `-----BEGIN CERTIFICATE-----
MIIDLjCCAhagAwIBAgIUMudqfWkqCviMATDYT0hPrrNis8MwDQYJKoZIhvcNAQEL
BQAwGDEWMBQGA1UEAwwNb2FtIHRlc3Qgcm9vdDAgFw0yNjA5MTQyMjEyMDJaGA8y
MTI2MDgyMTIyMTIwMlowIDEeMBwGA1UEAwwVb2FtIHRlc3QgaW50ZXJtZWRpYXRl
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAlMLt2Tc83khhnKz2EPrE
IUq/yhs/5+UCRLjfVOpbg99VcgNMM9MqiDPk99MsJgZP/s7zZdU6bZPddNMHVnGK
wTcf57CH0qp2gDLlIzZDbaspfuDPcQzFDJUSDN79vBLlT2nQOYDAstz8BHesYUCp
qWplXcSfFFz1+g7nxnKGElETMWo4RamroVr65rQjlvY9CudxJ6ufhds2cmCMflkj
fNWaa8ZpFFscGZLk4vcJp53bIi5utBHS40vQyBq8jbRH1aFuRUHlBewI+koYHLNA
F25y+KJp4Ukuj2YelyhErlTW3FjvRUEOrG6ZpVTRBWU4VtRHLeDaUyscoQd484U6
XQIDAQABo2YwZDASBgNVHRMBAf8ECDAGAQH/AgEAMA4GA1UdDwEB/wQEAwIBBjAd
BgNVHQ4EFgQUOl0ZkvpfMS20ACPsrp1AKLgUEO0wHwYDVR0jBBgwFoAUboe+IwrB
t8kAC8Akxaqo9HWIUa8wDQYJKoZIhvcNAQELBQADggEBAE1qkuY1u8RYdUQzBELl
GtVDxGg2aXp45L7w7UykqBvfd3laCqHgXa9WfT9HeN1jc+KuimoZAaV8/qACTLv9
zCg2nxx9KEmnaf2aRs4vajGVcHEav4IzR6bcSaCWVjSQSPTPWTuqRf98Jbfuq4gc
ZDerNmPGfzORoO3CVg6zILpvINyuHjtAvknoN1SvUPa7zz2mzwdSB0RnAYB/vsJi
GLUjfYAbCs/WwXoIClHOABnocrqueeAVg6KXqoea9eIGsuIElMJWl6jO/KHX7FNi
s6GZPLS0HhJnhLbegH+50+M7vbgfLE2zv6fA5z/8NwEnxdAy8fANsmYeEhclUpJI
4Zo=
-----END CERTIFICATE-----`;
const CHAIN_LEAF = `-----BEGIN CERTIFICATE-----
MIIDVDCCAjygAwIBAgIUQJdLLzB4vxRaQPgG+DsCcRC7RMIwDQYJKoZIhvcNAQEL
BQAwIDEeMBwGA1UEAwwVb2FtIHRlc3QgaW50ZXJtZWRpYXRlMCAXDTI2MDkxNDIy
MTIwMloYDzIxMjYwODIxMjIxMjAyWjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwggEi
MA0GCSqGSIb3DQEBAQUAA4IBDwAwggEKAoIBAQCiPXvsvJiqbU3lA0yWN4ahcYXQ
oM5+x97if51zX5HliN7xXyjzVy/UKJT4KP6WHs19EfLmR3lrTJ8fipsHgt5GjqSt
j8DAvdw7SAkKdUx+No5q0K5RYOuzoN4ises3DkJoK7ni2GZ+ceWG7ZPVn2K02DtS
s/gwQBzB1kZd4KsSCynA/cmGZlP5tOyi11ra70MDDKG4woEtlV7Bq5yxzj9b0uni
3yRg9yPb/fqlvRhkZL+Wd3gwxA6Q/GL3resfTza0mP1Jd2ytysy5BTQXOo9R9FE/
dojFEaXsQAqtbJ1RvtZL7/4jiOYDSQykNqnztdt2azRWWG1+T2n5tM7WSLBTAgMB
AAGjgY8wgYwwGgYDVR0RBBMwEYIJbG9jYWxob3N0hwR/AAABMAkGA1UdEwQCMAAw
DgYDVR0PAQH/BAQDAgWgMBMGA1UdJQQMMAoGCCsGAQUFBwMBMB0GA1UdDgQWBBQo
jysFd/48mW67dfkyDpbg4rxt1DAfBgNVHSMEGDAWgBQ6XRmS+l8xLbQAI+yunUAo
uBQQ7TANBgkqhkiG9w0BAQsFAAOCAQEAXBiTqB8Asib8SGyOVGl/epQbyPdAE8Ob
54AZ7zz0GFrpKffuMzu8L4dTuY6FMHTKCBr39F+odmLSy8kIyU0qr1gqRM8JAiew
Pp0rcgKmVyDhhWb19rUjqCKdEQ/gR3hv9CKVWIYhdvYa4/0KMVUUIBm19yCzXpH4
YCxB9nQiN5kheQY8dsa7zSc7/atg/SSPH+Ejelf1Gh0YndIl2DHdjQRzbxDaBBeP
F2qh+Mt+cKQXjASX857y5Vspov+VM76l2rz+HHjohVsy85g8BqRZiH93t/HyxHGq
R5zB/NPIde1UZ/542dcyIbJjUOoy7zOqLJEiwEljZjn+MLpzjy1o4A==
-----END CERTIFICATE-----`;
const CHAIN_LEAF_KEY = `-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQCiPXvsvJiqbU3l
A0yWN4ahcYXQoM5+x97if51zX5HliN7xXyjzVy/UKJT4KP6WHs19EfLmR3lrTJ8f
ipsHgt5GjqStj8DAvdw7SAkKdUx+No5q0K5RYOuzoN4ises3DkJoK7ni2GZ+ceWG
7ZPVn2K02DtSs/gwQBzB1kZd4KsSCynA/cmGZlP5tOyi11ra70MDDKG4woEtlV7B
q5yxzj9b0uni3yRg9yPb/fqlvRhkZL+Wd3gwxA6Q/GL3resfTza0mP1Jd2ytysy5
BTQXOo9R9FE/dojFEaXsQAqtbJ1RvtZL7/4jiOYDSQykNqnztdt2azRWWG1+T2n5
tM7WSLBTAgMBAAECggEAKDEaH7IzEdllOCxCj14vFJSmhWIo9cB3B15894WAA8CO
FnawEuSQ/TqWeQnS1AbKekb1iTXAryO6sdopAMnbXdhdlH+tzTHburXkQ3p+mi/S
xURwQsnDamsaTLcN4cQ/EEZw1PEuJvn5Vh1KB9xl3A5LV/gsrmtblGuMYBpV1vCX
CQ+XgMYtz1ByEuVsy8um2f52p/XQQHix+Xf1PCsqD+miMeCBI8fGJ/MZ9I+Lo9T/
4DqNC9emTVmVZgeKruPC/KmcDCtxcTtXCVPLm9lUCbonQFKRIODGmTmjAbYEg7+b
rDks9bqe43F5YoAl4jwnLi/Qc29iIN68jRYOZA6uWQKBgQDPCWVhXWr01jL4wcOb
p9oo7c0l7pSCl8Hw8uIPnTo+mUqmoMfCI/VIxmIJWPKQXL+btfLHkC1Teva36iKr
Ljyem4Z+niK4wjpV1dJZSwnGotu/NYtNbYvQRVTRJhj1ldo/EKTWBD0rxOzCAZhZ
ok7lnB2Sn64mNIISOuU2v1kV+wKBgQDIm/nP6kMdRLHvqe3KXBh5UaRbPvkdCVes
4HjbBu6+YjX76oNgwGsdkv3W1LnwuxLpwL0+wMqrBL4H1mF+M8+rTfDHmW82/PvA
we5WEaQu5gtNVpvneSQ5Qggx0gzwwEA2IqyjlCKCIejTaBJV/QcJns+cwiujMAYR
rvRfhTk3iQKBgFdghMvpzDkWqZ540GBCH/2EFz+6CC1xdOWG7EdguPMwaOQYGRZj
bKLRLxD43C53JlrGGHeG7so7rCKEiyspsWXTB5kqjkbhmhMd0c/jrnWJyCpTo74C
zK4ShLBcXs5O9zQEhzzXvyVYz/81AyJZMOkQ0R5PjAUNxhOBjblkWmm/AoGBAK1k
C07G901j47v3jx057rlldH6ddmm/enVk63C8lDwf3PMpZnaIucytERPPeAt3VkiO
G8QSlNmuVqWliUywcY1p53RNzQ+lJ+AafusLgnI6yYgGFOjEDygiR7zwBdlNAfMI
k1krn4wEzR10tWx3L88D4gRm25rH1mcQZg3ts1+pAoGASPeXeeZv3l6B84l/2Wgt
/TAPeIIeu2CapL+jzLl+N4fbVjgspV/pljm+cKtccSjDliKuxOTN3QQxZFD0Evrs
pa07vc3DKVo4AoemY0hWKgtFqGhSWqX8fMTe8ANWZOSK/3iXDI87tBHSQK/eOFZ4
7gF9ZQtXQ3prqz2YjsDVz1Y=
-----END PRIVATE KEY-----`;
const FUTURE_CA = `-----BEGIN CERTIFICATE-----
MIIDITCCAgmgAwIBAgIUNM/2tdc+Ywb9ZWvRRx4NNgFR/rkwDQYJKoZIhvcNAQEL
BQAwGDEWMBQGA1UEAwwNb2FtIGZ1dHVyZSBjYTAeFw0yNjA5MTQyMjEyMDJaFw0z
NjA5MTEyMjEyMDJaMBgxFjAUBgNVBAMMDW9hbSBmdXR1cmUgY2EwggEiMA0GCSqG
SIb3DQEBAQUAA4IBDwAwggEKAoIBAQC5qlBckZJoweBQQCgg1IYwxD8sQJBQH1hs
N6Ais9sTI56voKTPt8i1UDx7B7H4WPPQ6U4BDm4rO5zqmHkhgryneGr7elnUhSKz
FCXjl2ZAtVqnFmupe6V2DYdoDACcC+A/pYloJw9ygnin4ZLgKMlNZ7araj6lG6GE
kdREZyN/g12M/hm8Dj7q70h0NLsxXD6Cy2hNIrkLFtZGmbur+nK+brm4QB9EXNMF
TlqAO/drfqDWVgmbZXezYQyW2Eb6Ut3dxOBJTz6mVexjIiJnN9vasL2o7MfJG0BC
fCjXiCXX1f+lOaIz3PhBfxOQU0UDIugE4GK2GP/fyOA8O2n/8+tZAgMBAAGjYzBh
MB0GA1UdDgQWBBRB9w3DcL7yYXl7tjKj2Yi+489uITAfBgNVHSMEGDAWgBRB9w3D
cL7yYXl7tjKj2Yi+489uITAPBgNVHRMBAf8EBTADAQH/MA4GA1UdDwEB/wQEAwIB
BjANBgkqhkiG9w0BAQsFAAOCAQEAnRJH2lYCkbXfE6TCzlN0qyrPAzJ70AfAjAmA
gyUpV+lHa0GSEtOcaDCzfeswbt9rVX0sL2ts10NEI90Qg/J24519/DSeMZamrUDB
FkYT7BI40Qled7vkiHYaycmtjmKmCVshLvFj7tjdau8Dv9an628CEBoPcBBIpgjU
zIpLqpB/O9LBakfhjvIZ8GXJpBKS1EfFTbuaPO0xwBNfO6kKphzoe1GOpMOiUk1i
K3DID3twiccEZDZgBd9nFwaVhFLBGwzxOQFwO2zmYLbssiWczwKTcY8huj/dN6UP
wkiHB3xfT8msH3STYqcVk4gsZtS+Zh0SS18nw21wgZMJvtkSBA==
-----END CERTIFICATE-----`;
const FUTURE_LEAF = `-----BEGIN CERTIFICATE-----
MIIDBjCCAe6gAwIBAgIUYOYyslJDB9dovuLEJH+WSi4a31UwDQYJKoZIhvcNAQEL
BQAwGDEWMBQGA1UEAwwNb2FtIGZ1dHVyZSBjYTAiGA8yMTAwMDEwMTAwMDAwMFoY
DzIxMTAwMTAxMDAwMDAwWjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwggEiMA0GCSqG
SIb3DQEBAQUAA4IBDwAwggEKAoIBAQC5qlBckZJoweBQQCgg1IYwxD8sQJBQH1hs
N6Ais9sTI56voKTPt8i1UDx7B7H4WPPQ6U4BDm4rO5zqmHkhgryneGr7elnUhSKz
FCXjl2ZAtVqnFmupe6V2DYdoDACcC+A/pYloJw9ygnin4ZLgKMlNZ7araj6lG6GE
kdREZyN/g12M/hm8Dj7q70h0NLsxXD6Cy2hNIrkLFtZGmbur+nK+brm4QB9EXNMF
TlqAO/drfqDWVgmbZXezYQyW2Eb6Ut3dxOBJTz6mVexjIiJnN9vasL2o7MfJG0BC
fCjXiCXX1f+lOaIz3PhBfxOQU0UDIugE4GK2GP/fyOA8O2n/8+tZAgMBAAGjSDBG
MBoGA1UdEQQTMBGCCWxvY2FsaG9zdIcEfwAAATAJBgNVHRMEAjAAMB0GA1UdDgQW
BBRB9w3DcL7yYXl7tjKj2Yi+489uITANBgkqhkiG9w0BAQsFAAOCAQEAFOaoN/SL
i5wxBJnonptA/ixotKyiVgOsjaun3xakAcYM2gDQ0WidZPDivShP9MtRq0BSDpdQ
4EJ4DPQPSIZOeKxHunUrFwh8bFRZ0JM4HzWG+Xe59iKLcxlNsHJHVEJHCwFU2LaG
nV/igyxEVhpS72T8iKrnpZ4Yghcc5s6hvYvz66x/XBiSuoIDd7drKTw3Ki8FEKQk
70GV5MN3LMYeZjeb1CxvRrwpGWoO0+YEl8WjXDocVe1ICpxClnS1mrm9/ooCoExl
cVB2J26Xa2zTEgpcUBZtK82Fhk+t2AfJmeUL6Fr5tgrd+tmb/WCwB0l0KvsclmkX
2BOclqJ8JejPOA==
-----END CERTIFICATE-----`;
const FUTURE_LEAF_KEY = `-----BEGIN PRIVATE KEY-----
MIIEvwIBADANBgkqhkiG9w0BAQEFAASCBKkwggSlAgEAAoIBAQC5qlBckZJoweBQ
QCgg1IYwxD8sQJBQH1hsN6Ais9sTI56voKTPt8i1UDx7B7H4WPPQ6U4BDm4rO5zq
mHkhgryneGr7elnUhSKzFCXjl2ZAtVqnFmupe6V2DYdoDACcC+A/pYloJw9ygnin
4ZLgKMlNZ7araj6lG6GEkdREZyN/g12M/hm8Dj7q70h0NLsxXD6Cy2hNIrkLFtZG
mbur+nK+brm4QB9EXNMFTlqAO/drfqDWVgmbZXezYQyW2Eb6Ut3dxOBJTz6mVexj
IiJnN9vasL2o7MfJG0BCfCjXiCXX1f+lOaIz3PhBfxOQU0UDIugE4GK2GP/fyOA8
O2n/8+tZAgMBAAECggEARdIADYeq5N0/4z31OT9ixVUPoq8W9iKLiIq0nEhBsuVa
yBYj1H97KYAmdmfS7B9bdS0/adNI59YvsOMs7kaxdlMo/DArNunoPiruArQNPnlU
wXADhcVbWFVHHgAhfI1Uw+qXDUVfIENjZ1LDfqun5AWEIts9+q4049tJVX3p0gnr
L5KwN5/F5Lq8It3hBw0x2RWWky16TS7e4thRppp6q69sWlKvOcU1s2xDOzgWzVOz
ioHKt7p+nerjwNPE6HoT7IF8hNKXv4/j3GGdl5YzFg9kRZy2c2n16ugsXpcGkT5U
zIV+EEE5wKjiRZdPJpU6LZ2QAtzhtiuRZI8hDZXkBwKBgQDfUT6yLZRXNk7+ZJWq
9GgIHdy6FMVvkFxLGHTVtf4cVP+pnr5IWnbOXTWixI7oCnkeUYIRIJOD8v2fzw7p
bUTlUBpjwkDNhpwk22SDAl8SFKkl1jMRgSJDOMH2N3Smi4qxxG9GL5JKRP5FblDy
bkqQ/pvtJwINu8aqcIfaDgkVOwKBgQDU1mfGORu71bao5Afp8jG+He5AxY7tmNGi
R4NRnZanS8WeqvhuQx2Z07/g+Il6amwHUXbvPCAblR67Y6HQOWFAhqvKL1hPdQT+
nc22e5eLNqDNWqCy42yvD92C/TGyWUjWN8/qbzHZRs1Rlugk44uwfJYovZGVKITd
tHhSiHioewKBgQCu5ZNjupzGHOuLAz3QkO/1A2Y+ekwSzw3pZnMCeTFWAR/mOUQv
qGIJxyhdnPGLO8CWBSIHxeqiWalXArRcDs75hV3VqWpVTMp3dzfl/vJ0V6gN0Q9X
8znhSc9mxRHf6cOq6/x2DIXXEufNetN8uvI9UprOBlHubZTvIYjUN0/XxwKBgQCG
oq54rQ2HL7Thd4YuDmA7BJH/dTlpwV7zCcvfKBHx+DOloD+Q+HHUKifZ9z54KrP1
mSnkQiOJbzZGkcr9fh6wA8DOIE77zGmBa2+C/QGrNb5YyPiY0NaikyWrw+DZEjPK
Fvo2MWrWKDyfXReypiJqXRVb5jcepMgPuybWBrBU2wKBgQC2OZ6PmRYNEmtP8uWW
itFSl02i5ksjZ6ierj/fO6ON0dg4DjR/uGCcRKv595Il0ZoCf4OYGMUX1i0gxDaA
/uE3VnejLAASanSt1dJuBe2bEUioMdNJZAYz1FaYfjHuhN114Aly0NTuIa8Zgijv
vnmsjWZJwSDu/vIHNLARSmgUmw==
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

// --- a leaf chained through a non-self-signed intermediate -------------------
// The intermediate is signed by a root the client is not given, so no complete
// chain can be built: leaf+intermediate is "unable to get local issuer", the
// leaf alone with the intermediate as `ca` is "unable to get issuer", and only
// the root as `ca` verifies it.
const chainServer2 = await serve({ cert: CHAIN_LEAF + "\n" + CHAIN_INTER, key: CHAIN_LEAF_KEY });
await report("chain to untrusted root, no ca", chainServer2, {});
await report("chain to untrusted root, ca=root", chainServer2, { ca: CHAIN_ROOT });
chainServer2.close();

const leafOnlyServer = await serve({ cert: CHAIN_LEAF, key: CHAIN_LEAF_KEY });
await report("leaf only, ca=intermediate", leafOnlyServer, { ca: CHAIN_INTER });
leafOnlyServer.close();

// --- not yet valid: the validity period beats every other complaint ----------
const futureServer = await serve({ cert: FUTURE_LEAF, key: FUTURE_LEAF_KEY });
await report("not yet valid, ca", futureServer, { ca: FUTURE_CA });
await report("not yet valid, no ca", futureServer, {});
await report("not yet valid, ca, wrong host", futureServer, { ca: FUTURE_CA, servername: "example.com" });
await report("not yet valid, rejectUnauthorized:false", futureServer, { ca: FUTURE_CA, rejectUnauthorized: false });
futureServer.close();
