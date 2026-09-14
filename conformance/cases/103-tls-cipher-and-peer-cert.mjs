// socket.getProtocol(), getCipher(), getEphemeralKeyInfo(), getPeerCertificate()
// and getPeerX509Certificate() report what OpenSSL reports in Node (#138):
// "TLSv1.3", the OpenSSL cipher name next to the IANA standardName, {} for the
// key exchange of a TLS 1.3 handshake, and the legacy certificate object with
// Node's keys in Node's order. oam used to hand out rustls's Debug names
// ("TLSv1_3", "TLS13_AES_256_GCM_SHA384") and {} for the certificate, so code
// switching on the protocol string or pinning a peer fingerprint diverged.
//
// Three servers in one file, since the legacy object's key fields differ by
// key type: RSA carries modulus/bits/exponent/pubkey (the SPKI DER), EC
// carries bits/pubkey (the raw point)/asn1Curve/nistCurve, plus the SAN
// (with an IPv6 entry), infoAccess and ext_key_usage the EC fixture was
// issued with; the third serves a CA-signed leaf with its CA, so
// getPeerCertificate(true) links leaf -> CA -> itself. Self-signed fixtures
// and rejectUnauthorized:false on the client (nothing here depends on
// verification). The getters are read before the handshake, after it on
// both the client and the accepted server socket, and after 'close'.
import tls from "node:tls";

const RSA_CERT = `-----BEGIN CERTIFICATE-----
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
const RSA_KEY = `-----BEGIN PRIVATE KEY-----
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
const EC_CERT = `-----BEGIN CERTIFICATE-----
MIICjjCCAjOgAwIBAgIUXOZdy5EZsIcB1u7YHib8IKSt0ogwCgYIKoZIzj0EAwIw
JzESMBAGA1UEAwwJbG9jYWxob3N0MREwDwYDVQQKDAhPQU0gVGVzdDAeFw0yNjA5
MTQxMTQwMDlaFw0zNjA5MTExMTQwMDlaMCcxEjAQBgNVBAMMCWxvY2FsaG9zdDER
MA8GA1UECgwIT0FNIFRlc3QwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAARoQ+K9
2C1/U+oMMu9YeGgKq26iSMeaRY2M3U90rk513s4bDPUnyte8lj6ox7aIynK9/sRd
K+rvV5iup8hTjDBlo4IBOzCCATcwHQYDVR0OBBYEFIf6f7za8yL3hn9zuoMR8Sro
UP/mMB8GA1UdIwQYMBaAFIf6f7za8yL3hn9zuoMR8SroUP/mMA8GA1UdEwEB/wQF
MAMBAf8wVgYDVR0RBE8wTYIJbG9jYWxob3N0hwR/AAABhxAAAAAAAAAAAAAAAAAA
AAABgRBvYW1AZXhhbXBsZS50ZXN0hhZodHRwczovL2V4YW1wbGUudGVzdC94MGAG
CCsGAQUFBwEBBFQwUjAlBggrBgEFBQcwAYYZaHR0cDovL29jc3AuZXhhbXBsZS50
ZXN0LzApBggrBgEFBQcwAoYdaHR0cDovL2NhLmV4YW1wbGUudGVzdC9jYS5jcnQw
HQYDVR0lBBYwFAYIKwYBBQUHAwEGCCsGAQUFBwMCMAsGA1UdDwQEAwIHgDAKBggq
hkjOPQQDAgNJADBGAiEAw+qdvX6YFomEXdQPG1vSJBl47I7t7e8dMaNpoUNBGbUC
IQDi8RdNKrN+O1c/Akki7MgLI3ajlpybzMlfwAsjLw27eg==
-----END CERTIFICATE-----`;
const EC_KEY = `-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgPvDDIEVSJU6WHVBH
iloZy2/iYDqJmE94rSeR0TnYzwChRANCAARoQ+K92C1/U+oMMu9YeGgKq26iSMea
RY2M3U90rk513s4bDPUnyte8lj6ox7aIynK9/sRdK+rvV5iup8hTjDBl
-----END PRIVATE KEY-----`;
const LEAF_CERT = `-----BEGIN CERTIFICATE-----
MIIDeDCCAmCgAwIBAgIUWm9SUcIRPRhgJMKLa2koUHS2xtMwDQYJKoZIhvcNAQEL
BQAwMjELMAkGA1UEBhMCVVMxETAPBgNVBAoMCE9BTSBUZXN0MRAwDgYDVQQDDAdU
ZXN0IENBMB4XDTI2MDkxNDExMTIxNFoXDTM2MDkxMTExMTIxNFowOjESMBAGA1UE
AwwJbG9jYWxob3N0MREwDwYDVQQKDAhPQU0gVGVzdDERMA8GA1UECgwIU2Vjb25k
IE8wggEiMA0GCSqGSIb3DQEBAQUAA4IBDwAwggEKAoIBAQCvZmi12M3z1+rgg0WX
e6KTvMacHnqCDB86vMWsLmpEis7vlG1FCzbJglmLqEm2ezh2CrD6qX5ebrHHeBzD
9N6hTl7HiC+a6iQQIC/MmPZwULkCfeH0buEwofYKs4TsY5L6BdPO9R1DMy7bj50D
puDH2XrwlixmxBaF6lbG4Q3ru37cl9bOZ6fM38xyIdRsHOfBGEpYhA04IwwCHgiy
iycbNw11stjh/pLxufIs6IC9gJtMtUs054Lwbl5FFckVmgZb/9C/yCyqGnf122Lj
jalZpHG8KfTCnx8FA1pLsgw2BUfE+stFwYzIRUKEuM7knF1WTi2tkutm5NNw0DWP
N6R9AgMBAAGjfjB8MBoGA1UdEQQTMBGCCWxvY2FsaG9zdIcEfwAAATAJBgNVHRME
AjAAMBMGA1UdJQQMMAoGCCsGAQUFBwMBMB0GA1UdDgQWBBR/ecaB7hyGk0XWC4M9
KcI7NGCIXjAfBgNVHSMEGDAWgBT5bt4TsEwfzDeUQU2IIqf07dYoKDANBgkqhkiG
9w0BAQsFAAOCAQEADZG+rI1Y4/OMKmhy3mEBaCKs5EdIv8QW9OInE1VIL0kr1LF4
dkMGJemaj5dxzagXz4Y5e4qK/Qt9vu2YeEM8deND5cSUCWeYD9ksfsC6Yo2wM1+X
NmeTFXt+tZ6tAjrAcfy1efWdRkHXRmRxhA068Bg9+WQqRoyFAv1usiS2ji/dGhsT
ZcIjyDq7ppE+EIQ3Cyl7SoLqQEoWM4nXo/gl1swXL1jEeJBHOGyugGEyUT6A3Xp+
T+9f+hQU9pUGSnA0FxKez9p2qloULtNcuQI8FTBlLkqIjLlFVhIgYc9GrybgEcZz
sWzAKNllrZyIqRKhxZPdy9SJT2RPDN299G6wdw==
-----END CERTIFICATE-----`;
const CA_CERT = `-----BEGIN CERTIFICATE-----
MIIDRTCCAi2gAwIBAgIUZWBXiNnxahPBrN+4Oc8+hnskUDkwDQYJKoZIhvcNAQEL
BQAwMjELMAkGA1UEBhMCVVMxETAPBgNVBAoMCE9BTSBUZXN0MRAwDgYDVQQDDAdU
ZXN0IENBMB4XDTI2MDkxNDExMTIxNFoXDTM2MDkxMTExMTIxNFowMjELMAkGA1UE
BhMCVVMxETAPBgNVBAoMCE9BTSBUZXN0MRAwDgYDVQQDDAdUZXN0IENBMIIBIjAN
BgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAw1Sc6jaJpCV1SzxQ5EGC9z/AZdUS
Og6XI8qTbkcUTxoMYqlLtJ3VAo9o5miiei0w9y4pGPu3tRqg42K9e2gJokkZ4yLC
xExxBmBSZ4KSsGpAKQExsAkup3XN2MqELLOgVhqAJWXuvtnbQhVUtkPkR9aMBD6Y
62Y2ik04v3Ufd1mGXJSntSXkfCDi705fjOItlY8REw35N+HxptBvbIomdBwLAYOx
oPKfdfIVmuxm594SRJ0Dfj6biJumX69PEjUpPuDdVYK4FvFcNf6zIt04hMnfHDm4
rjQZq2TeczNFMIBXAPLjS+l3lqaj6FG0FsbIdHKp6hY1r9Y/qxjH5kTogwIDAQAB
o1MwUTAdBgNVHQ4EFgQU+W7eE7BMH8w3lEFNiCKn9O3WKCgwHwYDVR0jBBgwFoAU
+W7eE7BMH8w3lEFNiCKn9O3WKCgwDwYDVR0TAQH/BAUwAwEB/zANBgkqhkiG9w0B
AQsFAAOCAQEAUyUboZTClFMkAWbH42DHOijK8GJPxg/8wjal7O2McQQ+y5HVQzva
JUzxnK3FOfmkokbh4xBvqR2HCD5qBEG1bb8owu5cd9+m8FIs0IsaNjhXmWLjmP1x
KYe9tEmzN3H3IIbwJYECLv+3qNOfEA+VFB2L/+e96KRgowMhvDtB4inY3opnba9d
KPRrnVp1aJc0i6KJizs+Ba+XkZA5qL8qZpMoE1ExGv0yvaXgiY2Y5sI6tMhHJNzU
7dWBcLpBQESYuhXsX4dWcZR2TqNKI2Sc72XfurodDx6wLqt66cOtCtSKBpGsn0/D
l+tacbAs/ZF/7Hwrhdt/N6xypEi/dDQJAw==
-----END CERTIFICATE-----`;
const LEAF_KEY = `-----BEGIN PRIVATE KEY-----
MIIEvwIBADANBgkqhkiG9w0BAQEFAASCBKkwggSlAgEAAoIBAQCvZmi12M3z1+rg
g0WXe6KTvMacHnqCDB86vMWsLmpEis7vlG1FCzbJglmLqEm2ezh2CrD6qX5ebrHH
eBzD9N6hTl7HiC+a6iQQIC/MmPZwULkCfeH0buEwofYKs4TsY5L6BdPO9R1DMy7b
j50DpuDH2XrwlixmxBaF6lbG4Q3ru37cl9bOZ6fM38xyIdRsHOfBGEpYhA04IwwC
HgiyiycbNw11stjh/pLxufIs6IC9gJtMtUs054Lwbl5FFckVmgZb/9C/yCyqGnf1
22LjjalZpHG8KfTCnx8FA1pLsgw2BUfE+stFwYzIRUKEuM7knF1WTi2tkutm5NNw
0DWPN6R9AgMBAAECggEAGKJs/XFUR7WhHuRA/2wVYuOGD4I2WZKDRlgh+TNRqIvI
UZzKlgJjsPyWQAekRrVasjWBMstgXLn2TRohDCKVrBkaNbL6YKsW4o7qt7UaE586
xM9ST2bNSOvOZyVce2jmySfNXklN0VTcdWjfuBYVhuwUGLs2xD4xHaDSjD8qmdtx
MO95lxlhpvHWtbsI6EVcmDPHasjuXAXAN2AT5jK90ff9jkMhIAr5V9htZdDd9skI
OA/6DoLKAqA7bmAb8iJ79iRESbjBNpM6SPO3ScO7wRA5aAB5xJKiPfYZy6GiE3MK
yCAQUuEb8mEJpudKqMNkIm2EwjiVnbt0Z019i8dgIQKBgQDsAq6jNbqKoUl1DTeu
IN/4ayAfNMOtpDhFPtM+542QNZgIW+NyKnC2eozWVHQtQ0kXeEtpglnf+IZKDfb5
C59R88uq2P4OEvgpjA/M5fBrvAd3+Rz0HdbHf1Sjs51+IyS/lU73MD8iBmK1jWe7
EBOGpHcjCxTklrlzjaCO2RpzDQKBgQC+QYkfY3jDjKOhps88t1ZxVF+rWnoLpBnU
GG6PtnmXRD6X5uh9wT55P+Chr7OltvWc+Dd7attixABw7NwjHzrhHLNLdm0IbbV+
bTA3/CrY8Kz/GHro8jOM3Sg4BuMgXXeE/z10vpHJyv51qS3Z6MC3fn5wj5H8kcGZ
k8jRmspbMQKBgQDZq2+eH8O4cCDr0BD2jGOFHmg139hJohgz5Um3zqAFzSg3LWiM
tw/VfRm/44xy4ofbGZuT6CE0LGbOjiqmb021rAC/xfoqyNwQlZlNBRXEh1rsD9ng
XFTnEkzh3pr25zrRZ8e4u8q+et03TP/Ky3z2xWEL9QCEA29vX8Qhe6KlUQKBgQCO
gEmzb+7hEPLyvh1UzcF6SwcJMlBdbcFGwjH1hGhYK25ymiojHt2rNXQLxq1Y/svC
kYwE7cl6lXH7Iv3TdK3GNJf6eq459OpO0nueQ0rYiJQa0XwmBFsmM/PO2yG9eSRv
QjoGukI6Ecg72saUA6htB9qudmqS8Z0/aZitnjHY0QKBgQCJyCFv6XwC/jULjfdl
IQoQxWfXsICLQ+jEoCbdg6FITRXcRlp22U/ucmf66fToDNefCpkOUG3M42WhBe86
K0P/Guh98jAuquxrSBL2RryVotaRGXG06uk7QylEDJUSgDbNkgzNjm20JFy3stmM
l2eb728Ku35DtszRy9AJ/GxD+A==
-----END PRIVATE KEY-----`;

let section = "start";
const watchdog = setTimeout(() => {
  console.log("WATCHDOG " + section);
  process.exit(9);
}, 20000);

function show(label, value) {
  console.log(label + "=" + JSON.stringify(value));
}
// The legacy object with its Buffers as hex, so it prints byte for byte, and
// issuerCertificate inlined -- "<self>" where a certificate points at itself.
function legacy(cert) {
  const out = {};
  for (const key of Object.keys(cert)) {
    const v = cert[key];
    if (key === "issuerCertificate") out[key] = v === cert ? "<self>" : legacy(v);
    else if (Buffer.isBuffer(v)) out[key] = "hex:" + v.toString("hex");
    else out[key] = v;
  }
  return out;
}
function getters(s) {
  return {
    protocol: s.getProtocol(),
    cipher: String(s.getCipher()),
    ephemeral: s.getEphemeralKeyInfo(),
    peer: s.getPeerCertificate(),
    detailed: s.getPeerCertificate(true),
    x509: String(s.getPeerX509Certificate()),
  };
}

async function run(name, cert, key) {
  section = name;
  let accepted;
  const serverSide = new Promise((r) => { accepted = r; });
  const server = tls.createServer({ cert, key }, (s) => {
    s.on("data", (d) => s.write(d));
    accepted(s);
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  const port = server.address().port;

  const s = tls.connect({ host: "127.0.0.1", port, rejectUnauthorized: false, servername: "localhost" });
  show(name + ".before", getters(s));
  await new Promise((r) => s.once("secureConnect", r));

  show(name + ".protocol", s.getProtocol());
  show(name + ".cipher", s.getCipher());
  show(name + ".ephemeral", s.getEphemeralKeyInfo());
  const peer = s.getPeerCertificate();
  show(name + ".peer.keys", Object.keys(peer));
  show(name + ".peer", legacy(peer));
  show(name + ".peer.shape", [
    Object.getPrototypeOf(peer) === Object.prototype,
    Object.getPrototypeOf(peer.subject) === null,
    Object.getPrototypeOf(peer.issuer) === null,
    peer.infoAccess === undefined ? "no infoAccess" : Object.getPrototypeOf(peer.infoAccess) === null,
    typeof peer.bits,
    typeof peer.ca,
  ]);
  const detailed = s.getPeerCertificate(true);
  show(name + ".detailed", legacy(detailed));
  show(name + ".detailed.fresh", [detailed !== peer, "issuerCertificate" in peer, detailed.issuerCertificate === detailed]);
  const x509 = s.getPeerX509Certificate();
  show(name + ".x509", {
    ctor: x509.constructor.name,
    subject: x509.subject,
    issuer: x509.issuer,
    serialNumber: x509.serialNumber,
    validFrom: x509.validFrom,
    validTo: x509.validTo,
    fingerprint512: x509.fingerprint512,
    subjectAltName: String(x509.subjectAltName),
    infoAccess: String(x509.infoAccess),
    keyUsage: String(x509.keyUsage),
    sameFingerprint: x509.fingerprint256 === peer.fingerprint256,
    sameRaw: x509.raw.equals(peer.raw),
    fresh: s.getPeerX509Certificate() !== x509,
  });
  show(name + ".legacyObject.keys", Object.keys(x509.toLegacyObject()));

  // The accepted socket: the same protocol and cipher, no key info, and no
  // peer certificate (the client sent none).
  const serverSocket = await serverSide;
  show(name + ".server", getters(serverSocket));

  s.end();
  await new Promise((r) => s.once("close", r));
  show(name + ".closed", getters(s));
  serverSocket.destroy();
  await new Promise((r) => server.close(r));
}

await run("rsa", RSA_CERT, RSA_KEY);
await run("ec", EC_CERT, EC_KEY);
await run("chain", LEAF_CERT + "\n" + CA_CERT, LEAF_KEY);
clearTimeout(watchdog);
