// tls (#144, the residue of case 109): the cipher suite a pinned TLS 1.2
// handshake negotiates, the server's honorCipherOrder, the live
// tls.DEFAULT_MIN_VERSION / DEFAULT_MAX_VERSION defaults, and the
// --tls-min-v1.x / --tls-max-v1.x flags that set them.
//
// Node's OpenSSL offers its suites in tls.DEFAULT_CIPHERS' order -- under TLS
// 1.2 the ECDHE AES-128-GCM suites before AES-256-GCM, CHACHA20 last; under
// TLS 1.3 AES-256, CHACHA20, AES-128 -- and a server picks by its own list
// unless honorCipherOrder is given falsy (!!value, else true). oam's rustls
// used to offer AES-256 first, so two oam peers negotiated
// ECDHE-RSA-AES256-GCM-SHA384 where two Node peers negotiate
// ECDHE-RSA-AES128-GCM-SHA256, and case 109 left the cipher out for that
// reason. A null minVersion / maxVersion is the module's live DEFAULT_*
// value, validated like an explicit one (lib/internal/tls/common.js toV: the
// minimum first, both before the method name); TLS_method ignores the
// defaults, SSLv23_method keeps the default floor, a TLSv1_x_method pins x.
// --tls-min-v1.x / --tls-max-v1.x set the initial values, from argv or
// NODE_OPTIONS, with Node's precedence when several are given (the lowest
// minimum, the highest maximum). Every value here was measured on v22.22.2.
//
// Never printed: the cipher under honorCipherOrder: false against a client
// offering a different order (only a `ciphers` list would do that, and oam
// ignores it), and process.execArgv with two flags (Node keeps argv order,
// oam a fixed one; docs/node-divergences.md entry 34).
import tls from "node:tls";
import https from "node:https";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { execFileSync, spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const CERT = `-----BEGIN CERTIFICATE-----
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
const KEY = `-----BEGIN PRIVATE KEY-----
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

// A private CA (valid 100 years) and the localhost leaf it signed (SAN
// DNS:localhost, IP:127.0.0.1): what fetch() and an option-less https.get()
// can trust only through NODE_EXTRA_CA_CERTS, which the fetch child below
// gets.
const FETCH_CA = `-----BEGIN CERTIFICATE-----
MIIDMTCCAhmgAwIBAgIUDvtPdO4ljOTrt9v6/+Ds4F6Q6HgwDQYJKoZIhvcNAQEL
BQAwHzEdMBsGA1UEAwwUb2FtIGNhc2UgMTc4IHRlc3QgQ0EwIBcNMjYwOTIzMTgy
MjE0WhgPMjEyNjA4MzAxODIyMTRaMB8xHTAbBgNVBAMMFG9hbSBjYXNlIDE3OCB0
ZXN0IENBMIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAz9cdoNsHNOsj
GTcYfimeOeV9zQaJgGgongxaEDtri3QGl0uNjPUhgRtvIkVagRLH3Huohw/yv6p8
WhsTOdrJ8/OQ+kBTP/KNIkjjsVkXoPZDCt/FDa74EoNJZLJxAlF90nVFBiZzvB2B
nKHVZJjyhyETcAEbSynrHbK+SpMlrFxcNLAatbPPI08KnBRCK7Lj6rA+yt98o3kb
ZMbZ95i/OeBh/9gWCY/0GTTtIlLuG/Fgih7xYmyBXfITklrt3dzDL8Q9usb7IV6M
cqwgdb78c90nw20dMRZ+EG5NAV4tkCYrhli7vOuep4JBI49ZpKpPNHkFn2+rawY5
W+0PvYd5IQIDAQABo2MwYTAdBgNVHQ4EFgQU1XqMZ0VpqntEb9tcwFu+AzFxG2Uw
HwYDVR0jBBgwFoAU1XqMZ0VpqntEb9tcwFu+AzFxG2UwDwYDVR0TAQH/BAUwAwEB
/zAOBgNVHQ8BAf8EBAMCAQYwDQYJKoZIhvcNAQELBQADggEBAF+iugXNCBQDJpTg
Urq26/DoFTPtrK6u4DHcPx9XpNavTh+uLr3xDxfjmH9ozOTjPkfJURHhDkSmBath
fnr6RD9YjiZcrKAVA+V77dGo23MfeVa/xJnYHpXy2iuc4zm09s1KxYTOenw3+MKz
qvGMiAZqXsd4KWxCeplPEA+E/T1Ytm4mY+cLFrohxPwFJakXemL60HcB0zALKAsk
9U3s++koVXZ+olzVBNc6cDGNyEruzfKFzbSU7pXxqxGN0X3zwHlYoNB6SqpZuiUZ
78SnbUSI6SHu6okYRpS7Ezfk7MDWoGaXL1bET7WGX+Tc6CLLCYLIOvFiQ+7pkwgi
lWA6cC8=
-----END CERTIFICATE-----
`;
const FETCH_LEAF = `-----BEGIN CERTIFICATE-----
MIIDUDCCAjigAwIBAgIUEns5QKWKdI8bDzl7kmMBUZycEV8wDQYJKoZIhvcNAQEL
BQAwHzEdMBsGA1UEAwwUb2FtIGNhc2UgMTc4IHRlc3QgQ0EwIBcNMjYwOTIzMTgy
MjE0WhgPMjEyNjA4MzAxODIyMTRaMBQxEjAQBgNVBAMMCWxvY2FsaG9zdDCCASIw
DQYJKoZIhvcNAQEBBQADggEPADCCAQoCggEBAJhs8ruXknapFBnTKlK5vROHOizd
5cBQuFGljwnX/aFc8TLLX6OcHydZnzY0tg7GMxx7i6t0Becop0t+ZMCuxljUB4y7
RHUEm8G0dWJ1KdJZRIvHYnhGB6MdBKYapEzTloqGWuQBj42QZiyjzeJuczzueE2B
sxHrK+s83MzGOj9znftD6WkXBnWcMzsyovbsZjKru30OE+uuIgmrhtDYmLhpX9GS
WVo1szau8JSWBrnsH2jFrvu77YCJWOOFYPijJ5oyjYAD1AfpS/hTxgfZA9BZUxIo
NaEUisY7maKoORrd2kMOvv506onz7ig7adfsCpWHcoAbtFScWc9fyy81N0MCAwEA
AaOBjDCBiTAaBgNVHREEEzARgglsb2NhbGhvc3SHBH8AAAEwCQYDVR0TBAIwADAL
BgNVHQ8EBAMCBaAwEwYDVR0lBAwwCgYIKwYBBQUHAwEwHQYDVR0OBBYEFEuYio5R
kZRiCdH97/NbiO7pC0EUMB8GA1UdIwQYMBaAFNV6jGdFaap7RG/bXMBbvgMxcRtl
MA0GCSqGSIb3DQEBCwUAA4IBAQB4rZ898wcpW70moYgPGUp/RZN9ZjUt4PbgGGyU
p+4cfkDkMglb4b/NfXd6bGHFA0buxH7U+9lOU4pCGy23JkWgs8IMruB9fenbNAKv
eWCrljFLb217AmE7JnAr5cXl8Kwh/7Bga/JP5oKmS1UGiR5dl3yMk0iKJ3yd9ubr
cSMBxg2WAOFF+My6rYNR1f0Dp9/8lscZ/AErXybmlP1SXI8JDLJBLMDN/wLL4V1A
UDYCSZljuL4gmS3b0GF2Tdl5Z+EwGFIwZ6Y4DBHDeOlQ6AjTXTVOhyI3BTHS2hGo
QniMX7unLF0cGCBWEtOd0tDNbRkHDVcnhebY0dJjPY7+Y6+P
-----END CERTIFICATE-----`;
const FETCH_LEAF_KEY = `-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQCYbPK7l5J2qRQZ
0ypSub0Thzos3eXAULhRpY8J1/2hXPEyy1+jnB8nWZ82NLYOxjMce4urdAXnKKdL
fmTArsZY1AeMu0R1BJvBtHVidSnSWUSLx2J4RgejHQSmGqRM05aKhlrkAY+NkGYs
o83ibnM87nhNgbMR6yvrPNzMxjo/c537Q+lpFwZ1nDM7MqL27GYyq7t9DhPrriIJ
q4bQ2Ji4aV/RkllaNbM2rvCUlga57B9oxa77u+2AiVjjhWD4oyeaMo2AA9QH6Uv4
U8YH2QPQWVMSKDWhFIrGO5miqDka3dpDDr7+dOqJ8+4oO2nX7AqVh3KAG7RUnFnP
X8svNTdDAgMBAAECggEAFXGeZI3aaR84WLnAhori8tBfeths7jVs+O+VxAjDAeSV
elPqTJY2O877+yBHTKTNpAAtkh1shyzM/G33trPf67dIqJ/f7aaMUyAUM5nQHGu6
nP+b9tfDU0tN0CCHZNePokVsnA8sJvpdpYIWAPkQ9U2HV0Ab9TVkpF+XoKdyomJm
DjZuGXU2Lkm9gskdtrtxP9dEAYnKgQeDfgmn/i9bawn1xMH/p9fwKhVbaxmDSJFv
XGDugVZV/LjHK2H9xyFC9n92O+0tz32C7WVzD5C3t7YwLmFCEBZAOjzLFqQvsJ6U
zvmaHAajPXMw8C8+tEj4iyFgutLr6dVR5lb7xyA4AQKBgQDG6AasgC6MQv7fONv6
V9ld4PiuBAVj+cmbnUnIv2Wp04dTntEb5lXuQgmeiQrcnGrF50KUcaYXe4nYxwhE
ZDzqHRzo+U8PrxjFIQQGnARi9eMwDI96B31SN0l9wziOIQehMB0TQlWWheBHgzFu
HGxz/8bx3O1oqwClbWpDN+VHAQKBgQDELW/DTIz4tlZdUuWxnyc/F9JD+3ui30oF
qIvHZt/XZK2agWTTANL/NSGO5RcJoIogJaesoptaNeZnOo32An1IfZKRcLth7Taj
MUMS6Y9YUT7xBxyNXEZ4LxHydkee2QKbxAW+PLZk5SI+tQbniDJD84v5PwMEHffI
fpa8VEWiQwKBgQC6/m8nxOn92w4ZdS75T5V+eH3Rut4Ge1JaBajUHXvKCJ70sh4M
iKLIdzTr4hJgDH0kyKEDRUTMVsvlDFhtU38g6XXAYIE/UXGMAdnzDMHi9x86kNRh
+KCMpoVkwh9tHwg5NS5gaMBl3j5XfLL/vaEH/LJftz9KY1kcLJz1zJq0AQKBgDU8
w1yzlHoWOV/AFFdcgnELzOLoB0hO4i6g67XkRBCW4MnSHYNpcNkTGRVHNDZHm9RX
g6ZExnX3tJwE9utxB4C5myHe/ur3TeGBh9tFCMKF4dfU/zmZdgI9e9hZotwHtj6B
NrHGlhTRXba4t7PzcPihyjWMlQvz+f8t40gecnszAoGBALsQh4kp0BP6Hearxi02
gZg0AOeY2tRXnK+6GLyO4PUmrRtoOWrmXeoJyao15cL4tINE+IBaL5g3P5mo9XWB
erw2Wan8x7OmClXxc4mm3w2xWWoZEj3vJyhEauNfbJgt/yrGm1G1FK12hsWK1R1K
oMWl7y+w2KJlXjTSc0SiM5Qr
-----END PRIVATE KEY-----`;

const here = fileURLToPath(import.meta.url);

// The child the fetch section spawns under NODE_EXTRA_CA_CERTS: a server
// that pins its own range, so the client alone decides, then fetch() and an
// option-less https.get() under the live defaults. Node's undici and its
// https.Agent connect through tls.connect and read the defaults for every
// connection; oam's shared transport used to be built once with every
// version. An https.get whose floor is above the server's ceiling fails
// with the failed write's `write EPROTO`, its head having been queued
// behind the handshake the alert refused (case 179).
if (process.env.OAM_CASE_178 === "fetch") {
  let seen = "none";
  const conns = new Set();
  const serve = (range) => {
    const server = tls.createServer({ cert: FETCH_LEAF, key: FETCH_LEAF_KEY, ...range }, (c) => {
      conns.add(c);
      c.on("error", () => {});
      let buf = "";
      c.on("data", (d) => {
        buf += d.toString("latin1");
        if (buf.includes("\r\n\r\n")) {
          seen = c.getProtocol() + " " + c.getCipher().name;
          c.end("HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length: 2\r\n\r\nok");
        }
      });
    });
    server.on("tlsClientError", () => {});
    return new Promise((r) => server.listen(0, "127.0.0.1", () => r(server)));
  };
  const viaFetch = async (label, port) => {
    seen = "none";
    try {
      const res = await fetch("https://localhost:" + port + "/");
      await res.text();
      console.log(label + " fetch -> " + seen);
    } catch (e) {
      console.log(label + " fetch -> " + e.constructor.name + " " + e.message + " | cause " +
        (e.cause && e.cause.code) + " " + (e.cause && e.cause.constructor.name));
    }
  };
  const viaGet = (label, port, opts) => new Promise((resolve) => {
    seen = "none";
    let req;
    try {
      req = https.get("https://localhost:" + port + "/", { agent: false, ...opts }, (res) => {
        res.resume();
        res.on("end", () => { console.log(label + " https.get -> " + seen); resolve(); });
      });
    } catch (e) { console.log(label + " https.get -> THROW " + e.code); resolve(); return; }
    req.on("error", (e) => { console.log(label + " https.get -> ERROR " + e.code + " syscall=" + e.syscall); resolve(); });
  });
  const closeAll = async (server) => {
    for (const c of conns) c.destroy();
    conns.clear();
    await new Promise((r) => server.close(r));
  };
  let server = await serve({ minVersion: "TLSv1.2", maxVersion: "TLSv1.3" });
  let port = server.address().port;
  console.log("defaults " + tls.DEFAULT_MIN_VERSION + " " + tls.DEFAULT_MAX_VERSION);
  await viaFetch("default", port);
  await viaGet("default", port, {});
  if (process.env.OAM_CASE_178_FETCH !== "defaults") {
    tls.DEFAULT_MAX_VERSION = "TLSv1.2";
    await viaFetch("DEFAULT_MAX=1.2", port);
    await viaGet("DEFAULT_MAX=1.2", port, {});
    await viaGet("DEFAULT_MAX=1.2 maxVersionNull", port, { maxVersion: null });
    tls.DEFAULT_MAX_VERSION = "TLSv1.3";
    tls.DEFAULT_MIN_VERSION = "TLSv1.3";
    await viaFetch("DEFAULT_MIN=1.3", port);
    tls.DEFAULT_MIN_VERSION = "TLSv1.2";
    tls.DEFAULT_MAX_VERSION = "TLSv1.1";
    await viaFetch("DEFAULT_MAX=1.1", port);
    await viaGet("DEFAULT_MAX=1.1", port, {});
    tls.DEFAULT_MAX_VERSION = "bogus";
    await viaFetch("DEFAULT_MAX=bogus", port);
    await viaGet("DEFAULT_MAX=bogus", port, {});
    tls.DEFAULT_MAX_VERSION = "TLSv1.3";
    await closeAll(server);
    server = await serve({ minVersion: "TLSv1.2", maxVersion: "TLSv1.2" });
    port = server.address().port;
    tls.DEFAULT_MIN_VERSION = "TLSv1.3";
    await viaFetch("DEFAULT_MIN=1.3 vsServer1.2", port);
    await viaGet("DEFAULT_MIN=1.3 vsServer1.2", port, {});
    tls.DEFAULT_MIN_VERSION = "TLSv1.2";
    await viaFetch("restored vsServer1.2", port);
  }
  await closeAll(server);
  process.exit(0);
}

// A tls server; the socket is drained and its errors swallowed.
function listen(serverOpts) {
  const server = tls.createServer({ cert: CERT, key: KEY, ...serverOpts }, (s) => {
    s.resume();
    s.on("error", () => {});
  });
  server.on("tlsClientError", () => {});
  server.on("error", () => {});
  return new Promise((r) => server.listen(0, "127.0.0.1", () => r(server)));
}

// What a client negotiates: the protocol and the suite, by OpenSSL's name,
// the IANA name and the version getCipher() reports it under; or the error
// code (the OpenSSL messages diverge by design, see case 109).
function connectTo(port, clientOpts) {
  return new Promise((resolve) => {
    let c;
    try {
      c = tls.connect(
        { host: "127.0.0.1", port, rejectUnauthorized: false, servername: "localhost", ...clientOpts },
        () => {
          const ci = c.getCipher();
          resolve(c.getProtocol() + " " + ci.name + " " + ci.standardName + " " + ci.version);
          c.destroy();
        },
      );
      c.on("error", (e) => resolve("ERROR " + e.code));
    } catch (e) {
      resolve("THROW " + e.code);
    }
  });
}

// The child a flag test spawns: the defaults the flags set, execArgv (unless
// asked not to), and what a default handshake negotiates.
if (process.env.OAM_CASE_178 === "child") {
  const server = await listen({});
  const result = await connectTo(server.address().port, {});
  await new Promise((r) => server.close(r));
  const argv = process.env.OAM_CASE_178_ARGV === "no" ? "" : " " + JSON.stringify(process.execArgv);
  console.log(tls.DEFAULT_MIN_VERSION + " " + tls.DEFAULT_MAX_VERSION + argv + " " + result);
  process.exit(0);
}

let section = "start";
const watchdog = setTimeout(() => {
  console.log("WATCHDOG " + section);
  process.exit(9);
}, 60000);

async function negotiated(label, serverOpts, clientOpts) {
  section = label;
  const server = await listen(serverOpts);
  console.log(label + " -> " + (await connectTo(server.address().port, clientOpts)));
  await new Promise((r) => server.close(r));
}

// A synchronous throw: code, message and class, byte-identical.
function throws(label, fn) {
  section = label;
  try {
    const r = fn();
    if (r && r.close) r.close();
    if (r && r.destroy) r.destroy();
    console.log(label + " NO THROW");
  } catch (e) {
    console.log(label + " " + e.code + " | " + e.message + " | " + (e instanceof TypeError));
  }
}
const connectThrows = (label, opts) =>
  throws(label, () => tls.connect({ host: "127.0.0.1", port: 1, ...opts }, () => {}).on("error", () => {}));

// ---- the suite of a pinned TLS 1.2 handshake (case 109 prints the protocol only)
await negotiated("clientMax1.2", {}, { maxVersion: "TLSv1.2" });
await negotiated("clientPin1.2", {}, { minVersion: "TLSv1.2", maxVersion: "TLSv1.2" });
await negotiated("clientSp1.2", {}, { secureProtocol: "TLSv1_2_method" });
await negotiated("serverMax1.2", { maxVersion: "TLSv1.2" }, {});
await negotiated("serverSp1.2", { secureProtocol: "TLSv1_2_method" }, {});
await negotiated("default", {}, {});
// Both peers offer the same list, so the server's choice is the client's
// whichever order it honours.
await negotiated("serverMax1.2honorFalse", { maxVersion: "TLSv1.2", honorCipherOrder: false }, {});
section = "httpsServerMax1.2";
{
  const server = https.createServer({ cert: CERT, key: KEY, maxVersion: "TLSv1.2" }, (req, res) => res.end("ok"));
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  console.log("httpsServerMax1.2 -> " + (await connectTo(server.address().port, {})));
  await new Promise((r) => server.close(r));
}

// ---- honorCipherOrder as the server reflects it: !!value when given, else true
section = "honorCipherOrder";
{
  const reflected = (opts) => JSON.stringify(tls.createServer({ cert: CERT, key: KEY, ...opts }).honorCipherOrder);
  console.log(
    "honorCipherOrder default=" + reflected({}) + " true=" + reflected({ honorCipherOrder: true }) +
      " false=" + reflected({ honorCipherOrder: false }) + " 0=" + reflected({ honorCipherOrder: 0 }) +
      " empty=" + reflected({ honorCipherOrder: "" }) + " null=" + reflected({ honorCipherOrder: null }) +
      " no=" + reflected({ honorCipherOrder: "no" }) + " 1=" + reflected({ honorCipherOrder: 1 }),
  );
  const server = tls.createServer({ cert: CERT, key: KEY });
  server.setSecureContext({ cert: CERT, key: KEY, honorCipherOrder: false });
  const flipped = server.honorCipherOrder;
  server.setSecureContext({ cert: CERT, key: KEY });
  console.log("setSecureContext honorCipherOrder=" + flipped + " then reset=" + server.honorCipherOrder);
}

// ---- the live defaults
console.log("defaults " + tls.DEFAULT_MIN_VERSION + " " + tls.DEFAULT_MAX_VERSION);
tls.DEFAULT_MAX_VERSION = "TLSv1.2";
await negotiated("DEFAULT_MAX=1.2 default", {}, {});
await negotiated("DEFAULT_MAX=1.2 clientExplicit1.3", {}, { maxVersion: "TLSv1.3" });
await negotiated("DEFAULT_MAX=1.2 bothExplicit1.3", { maxVersion: "TLSv1.3" }, { maxVersion: "TLSv1.3" });
await negotiated("DEFAULT_MAX=1.2 clientMaxNull", { maxVersion: "TLSv1.3" }, { maxVersion: null });
await negotiated("DEFAULT_MAX=1.2 bothTLS_method", { secureProtocol: "TLS_method" }, { secureProtocol: "TLS_method" });
await negotiated("DEFAULT_MAX=1.2 bothSSLv23", { secureProtocol: "SSLv23_method" }, { secureProtocol: "SSLv23_method" });
tls.DEFAULT_MAX_VERSION = "TLSv1.3";
tls.DEFAULT_MIN_VERSION = "TLSv1.3";
await negotiated("DEFAULT_MIN=1.3 default", {}, {});
await negotiated("DEFAULT_MIN=1.3 vsServer1.2", { minVersion: "TLSv1.2", maxVersion: "TLSv1.2" }, {});
await negotiated("DEFAULT_MIN=1.3 serverDefaultVsClient1.2", {}, { minVersion: "TLSv1.2", maxVersion: "TLSv1.2" });
await negotiated("DEFAULT_MIN=1.3 clientMax1.2", { minVersion: "TLSv1.2", maxVersion: "TLSv1.3" }, { maxVersion: "TLSv1.2" });
await negotiated("DEFAULT_MIN=1.3 clientSSLv23", { minVersion: "TLSv1.2", maxVersion: "TLSv1.3" }, { secureProtocol: "SSLv23_method" });
await negotiated("DEFAULT_MIN=1.3 clientTLS_methodVsServer1.2", { minVersion: "TLSv1.2", maxVersion: "TLSv1.2" }, { secureProtocol: "TLS_method" });
await negotiated("DEFAULT_MIN=1.3 clientTLSv1_2VsServer1.2", { minVersion: "TLSv1.2", maxVersion: "TLSv1.2" }, { secureProtocol: "TLSv1_2_method" });
tls.DEFAULT_MIN_VERSION = "TLSv1";
await negotiated("DEFAULT_MIN=TLSv1 default", {}, {});
tls.DEFAULT_MAX_VERSION = "TLSv1.1";
await negotiated("DEFAULT_MAX=1.1 clientDefaultVsServer1.2-1.3", { minVersion: "TLSv1.2", maxVersion: "TLSv1.3" }, {});
tls.DEFAULT_MIN_VERSION = "TLSv1.2";
tls.DEFAULT_MAX_VERSION = "TLSv1.3";

// ---- invalid defaults: validated like an explicit value, wherever a context is built
for (const [name, value] of [["bogus", "bogus"], ["771", 771], ["null", null], ["undefined", undefined], ["lowercase", "tlsv1.2"]]) {
  tls.DEFAULT_MAX_VERSION = value;
  connectThrows("DEFAULT_MAX=" + name + " connect", {});
  throws("DEFAULT_MAX=" + name + " createServer", () => tls.createServer({ cert: CERT, key: KEY }));
  throws("DEFAULT_MAX=" + name + " createSecureContext", () => tls.createSecureContext({}));
  throws("DEFAULT_MAX=" + name + " https.request", () => https.request({ host: "127.0.0.1", port: 1 }, () => {}).on("error", () => {}));
  connectThrows("DEFAULT_MAX=" + name + " explicitMax", { maxVersion: "TLSv1.3" });
  connectThrows("DEFAULT_MAX=" + name + " TLS_method", { secureProtocol: "TLS_method" });
  connectThrows("DEFAULT_MAX=" + name + " TLSv1_2_method", { secureProtocol: "TLSv1_2_method" });
  connectThrows("DEFAULT_MAX=" + name + " unknownMethod", { secureProtocol: "no_such_method" });
  connectThrows("DEFAULT_MAX=" + name + " conflict", { secureProtocol: "TLSv1_2_method", minVersion: "TLSv1.2" });
  connectThrows("DEFAULT_MAX=" + name + " badMin", { minVersion: "TLSv9" });
}
tls.DEFAULT_MAX_VERSION = "TLSv1.3";
tls.DEFAULT_MIN_VERSION = "bogusmin";
connectThrows("DEFAULT_MIN=bogusmin connect", {});
connectThrows("DEFAULT_MIN=bogusmin explicitMin", { minVersion: "TLSv1.2" });
tls.DEFAULT_MAX_VERSION = "bogusmax";
connectThrows("DEFAULT_MIN=bogusmin DEFAULT_MAX=bogusmax connect", {});
connectThrows("DEFAULT_MIN=bogusmin DEFAULT_MAX=bogusmax explicitMin", { minVersion: "TLSv1.2" });
tls.DEFAULT_MIN_VERSION = "TLSv1.2";
tls.DEFAULT_MAX_VERSION = "TLSv1.3";
console.log("defaults restored " + tls.DEFAULT_MIN_VERSION + " " + tls.DEFAULT_MAX_VERSION);

// ---- the value in the message is util.format's %j: a function or symbol is
// "undefined", a circular object "[Circular]", and a BigInt is JSON's own
// refusal, thrown as it is (a plain TypeError, no code)
connectThrows("minFunction", { minVersion: function f() {} });
connectThrows("minSymbol", { minVersion: Symbol("v") });
{
  const loop = {};
  loop.self = loop;
  connectThrows("minCircular", { minVersion: loop });
}
connectThrows("minBigInt", { minVersion: 1n });
connectThrows("conflictSymbolSp", { secureProtocol: Symbol("sp"), minVersion: "TLSv1.2" });

// ---- new tls.TLSSocket(socket, options) builds its context at construction
// (a socket that never connects included), unless given one
section = "TLSSocket";
const tlsSocketThrows = (label, opts) => throws(label, () => new tls.TLSSocket(null, opts));
tlsSocketThrows("TLSSocket badMin", { minVersion: "TLSv9" });
tlsSocketThrows("TLSSocket conflict", { secureProtocol: "TLSv1_2_method", maxVersion: "TLSv1.2" });
tlsSocketThrows("TLSSocket badMethod", { secureProtocol: "no_such_method" });
tlsSocketThrows("TLSSocket ok", { minVersion: "TLSv1.2" });
tlsSocketThrows("TLSSocket noOptions", undefined);
tls.DEFAULT_MAX_VERSION = "bogus";
tlsSocketThrows("TLSSocket bogusDefault", {});
tlsSocketThrows("TLSSocket bogusDefault noOptions", undefined);
tlsSocketThrows("TLSSocket bogusDefault explicitMax", { maxVersion: "TLSv1.3" });
tls.DEFAULT_MAX_VERSION = "TLSv1.3";
{
  const context = tls.createSecureContext({});
  tls.DEFAULT_MAX_VERSION = "bogus";
  tlsSocketThrows("TLSSocket bogusDefault secureContext", { secureContext: context });
  tls.DEFAULT_MAX_VERSION = "TLSv1.3";
}

// ---- the flags, through a child running this file. The spawns are
// synchronous, so the watchdog above cannot fire in this section: each child
// carries its own bound instead (a stuck one throws ETIMEDOUT here rather
// than leaving the harness to time the whole case out).
section = "flags";
const CHILD_TIMEOUT = 20000;
const child = (flags, env, showArgv) =>
  execFileSync(process.execPath, [...flags, here], {
    env: { ...process.env, ...env, OAM_CASE_178: "child", OAM_CASE_178_ARGV: showArgv ? "yes" : "no" },
    encoding: "utf8",
    timeout: CHILD_TIMEOUT,
  }).trim();
console.log("--tls-max-v1.2: " + child(["--tls-max-v1.2"], {}, true));
console.log("--tls-max-v1.3: " + child(["--tls-max-v1.3"], {}, true));
console.log("--tls-min-v1.0: " + child(["--tls-min-v1.0"], {}, true));
console.log("--tls-min-v1.1: " + child(["--tls-min-v1.1"], {}, true));
console.log("--tls-min-v1.2: " + child(["--tls-min-v1.2"], {}, true));
console.log("--tls-min-v1.3: " + child(["--tls-min-v1.3"], {}, true));
console.log("NODE_OPTIONS=--tls-max-v1.2: " + child([], { NODE_OPTIONS: "--tls-max-v1.2" }, true));
console.log("NODE_OPTIONS=--tls-min-v1.3: " + child([], { NODE_OPTIONS: "--tls-min-v1.3" }, true));
// Several: Node's precedence, from argv and NODE_OPTIONS alike.
console.log("--tls-min-v1.3 --tls-min-v1.0: " + child(["--tls-min-v1.3", "--tls-min-v1.0"], {}, false));
console.log("--tls-min-v1.1 --tls-min-v1.2: " + child(["--tls-min-v1.1", "--tls-min-v1.2"], {}, false));
console.log("--tls-max-v1.2 --tls-max-v1.3: " + child(["--tls-max-v1.2", "--tls-max-v1.3"], {}, false));
// The one pair Node refuses at startup (src/node_options.cc): a floor of
// 1.3 with a ceiling of 1.2, from argv or NODE_OPTIONS alike, exit 9 and
// nothing run. The message follows the executable's path, so only its
// text is checked.
const refused = (flags, env) => {
  const r = spawnSync(process.execPath, [...flags, here], {
    env: { ...process.env, ...env, OAM_CASE_178: "child", OAM_CASE_178_ARGV: "no" },
    encoding: "utf8",
    timeout: CHILD_TIMEOUT,
  });
  return "status=" + r.status + " stdout=" + JSON.stringify(r.stdout.trim()) + " refused=" +
    r.stderr.includes(": either --tls-min-v1.3 or --tls-max-v1.2 can be used, not both");
};
console.log("--tls-min-v1.3 --tls-max-v1.2: " + refused(["--tls-min-v1.3", "--tls-max-v1.2"], {}));
console.log("--tls-max-v1.2 --tls-min-v1.3: " + refused(["--tls-max-v1.2", "--tls-min-v1.3"], {}));
console.log("--tls-min-v1.3 --tls-max-v1.2 --tls-max-v1.3: " + refused(["--tls-min-v1.3", "--tls-max-v1.2", "--tls-max-v1.3"], {}));
console.log("NODE_OPTIONS=--tls-min-v1.3 --tls-max-v1.2: " + refused(["--tls-max-v1.2"], { NODE_OPTIONS: "--tls-min-v1.3" }));
console.log("NODE_OPTIONS=--tls-min-v1.3 --tls-max-v1.2 (both env): " + refused([], { NODE_OPTIONS: "--tls-min-v1.3 --tls-max-v1.2" }));
console.log("--tls-min-v1.2 --tls-max-v1.2: " + child(["--tls-min-v1.2", "--tls-max-v1.2"], {}, false));
console.log("--tls-min-v1.3 --tls-max-v1.3: " + child(["--tls-min-v1.3", "--tls-max-v1.3"], {}, false));
console.log("NODE_OPTIONS=--tls-max-v1.2 --tls-max-v1.3: " + child(["--tls-max-v1.3"], { NODE_OPTIONS: "--tls-max-v1.2" }, true));
console.log("NODE_OPTIONS=--tls-max-v1.3 --tls-max-v1.2: " + child(["--tls-max-v1.2"], { NODE_OPTIONS: "--tls-max-v1.3" }, true));
console.log("NODE_OPTIONS=--tls-min-v1.3 --tls-min-v1.0: " + child(["--tls-min-v1.0"], { NODE_OPTIONS: "--tls-min-v1.3" }, true));

// ---- fetch() and an option-less https.get(), in a child that trusts the
// private CA through NODE_EXTRA_CA_CERTS (written to a file it alone reads)
section = "fetch";
const caFile = path.join(os.tmpdir(), "oam-case-178-ca-" + process.pid + ".pem");
fs.writeFileSync(caFile, FETCH_CA);
try {
  const fetchChild = (flags, env) =>
    execFileSync(process.execPath, [...flags, here], {
      env: { ...process.env, ...env, NODE_EXTRA_CA_CERTS: caFile, OAM_CASE_178: "fetch" },
      encoding: "utf8",
      timeout: CHILD_TIMEOUT,
    }).trim();
  console.log("fetch under the live defaults:\n" + fetchChild([], { OAM_CASE_178_FETCH: "all" }));
  console.log("fetch under --tls-max-v1.2:\n" + fetchChild(["--tls-max-v1.2"], { OAM_CASE_178_FETCH: "defaults" }));
  console.log("fetch under NODE_OPTIONS=--tls-max-v1.2:\n" + fetchChild([], { NODE_OPTIONS: "--tls-max-v1.2", OAM_CASE_178_FETCH: "defaults" }));
} finally {
  fs.unlinkSync(caFile);
}

clearTimeout(watchdog);
