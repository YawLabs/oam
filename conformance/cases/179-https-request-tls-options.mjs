// https.request on the VERIFYING path (rejectUnauthorized not false) with
// per-request TLS options, against a server whose certificate chains to a
// private root (#146): `ca`; `maxVersion` / `secureProtocol` / `minVersion`;
// `servername`; `checkServerIdentity`; a `secureContext`; the URL form of
// https.get; the options of an https.Agent (a request's own, a pooled one
// reused, a keep-alive one reused, https.globalAgent.options) -- and what
// the SERVER negotiated, which is the only witness of the version pin.
//
// Then the shape a refused handshake takes. A server capped at TLS 1.2
// answers a floor of 1.3 with the protocol_version alert: a tls.connect
// socket fails with the alert's own code -- unless a write was queued
// behind the handshake (a `write()` on the line after `tls.connect()`, an
// `end('data')`), which OpenSSL then refuses: the socket's error and every
// queued write's callback are that write's, `write EPROTO` with errno, code
// and syscall (in that key order). `end()` with no data queues no write, so
// the alert's code stands. An https request's head is such a write, so
// https.request reports EPROTO once end(), write() or flushHeaders() has
// run, and the alert's code if the alert lands before any of them.
//
// Every value measured on Node v22.22.2. Up to 0.15.x oam sent a verifying
// https request over one shared client that applied none of these options;
// it then reported the alert's own code where node reports the failed
// write's. Never printed: errno (platform-specific) and the messages
// (OpenSSL's diagnostics; docs/node-divergences.md, entry 34).
import tls from "node:tls";
import https from "node:https";

// A private CA (valid 100 years) and the localhost leaf it signed (SAN
// DNS:localhost, IP:127.0.0.1) -- case 178's pair.
const ROOT = `-----BEGIN CERTIFICATE-----
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
const LEAF = `-----BEGIN CERTIFICATE-----
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
const LEAF_KEY = `-----BEGIN PRIVATE KEY-----
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

let section = "start";
const watchdog = setTimeout(() => {
  console.log("WATCHDOG " + section);
  process.exit(9);
}, 60000);

// A tls server speaking just enough HTTP/1.1 to answer, recording what each
// connection negotiated.
let seen = "none";
const conns = new Set();
function serve(opts) {
  const server = tls.createServer({ cert: LEAF, key: LEAF_KEY, ...opts }, (c) => {
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
}
async function closeAll(server) {
  for (const c of conns) c.destroy();
  conns.clear();
  await new Promise((r) => server.close(r));
}
// An error's shape without its platform errno or OpenSSL message: the code,
// the syscall, whether an errno is there, and the key order.
const shape = (e) => e.code + " syscall=" + e.syscall + " errno=" + (typeof e.errno === "number" && e.errno < 0 ? "negative" : String(e.errno)) + " keys=" + JSON.stringify(Object.keys(e));

// A verifying request; `viaUrl` uses https.get's URL form.
function request(label, port, opts, viaUrl) {
  section = label;
  seen = "none";
  return new Promise((resolve) => {
    let r;
    const done = (res) => {
      res.resume();
      res.on("end", () => { console.log(label + " -> " + res.statusCode + " " + seen); resolve(); });
    };
    try {
      r = viaUrl
        ? https.get("https://localhost:" + port + "/", opts, done)
        : https.request({ host: "localhost", port, path: "/", ...opts }, done);
    } catch (e) {
      console.log(label + " -> THROW " + e.code);
      resolve();
      return;
    }
    r.on("error", (e) => { console.log(label + " -> ERROR " + e.code); resolve(); });
    if (!viaUrl) r.end();
  });
}

// ---- the options, against a server offering 1.2 and 1.3
let server = await serve({ minVersion: "TLSv1.2", maxVersion: "TLSv1.3" });
let port = server.address().port;
await request("no options (private root)", port, {});
await request("ca", port, { ca: ROOT });
await request("ca + agent false", port, { ca: ROOT, agent: false });
await request("ca + maxVersion 1.2", port, { ca: ROOT, maxVersion: "TLSv1.2" });
await request("ca + secureProtocol TLSv1_2_method", port, { ca: ROOT, secureProtocol: "TLSv1_2_method" });
await request("ca + minVersion 1.3", port, { ca: ROOT, minVersion: "TLSv1.3" });
await request("ca + servername", port, { ca: ROOT, servername: "localhost" });
await request("ca + checkServerIdentity accepts", port, { ca: ROOT, checkServerIdentity: () => undefined });
await request("ca + checkServerIdentity refuses", port, { ca: ROOT, checkServerIdentity: () => { const e = new Error("no"); e.code = "PINNED_ELSEWHERE"; return e; } });
await request("secureContext with ca", port, { secureContext: tls.createSecureContext({ ca: ROOT }) });
await request("https.get url + ca", port, { ca: ROOT }, true);
await request("https.get url + ca + maxVersion 1.2", port, { ca: ROOT, maxVersion: "TLSv1.2" }, true);
const pooled = new https.Agent({ ca: ROOT });
await request("Agent ca", port, { agent: pooled });
await request("Agent ca, second request", port, { agent: pooled });
const keepAlive = new https.Agent({ ca: ROOT, keepAlive: true });
await request("Agent ca keepAlive, first", port, { agent: keepAlive });
await request("Agent ca keepAlive, second", port, { agent: keepAlive });
keepAlive.destroy();
await request("Agent ca + maxVersion 1.2", port, { agent: new https.Agent({ ca: ROOT, maxVersion: "TLSv1.2" }) });
await request("Agent ca, request maxVersion 1.2", port, { agent: pooled, maxVersion: "TLSv1.2" });
await request("Agent maxVersion 1.2, request ca", port, { agent: new https.Agent({ maxVersion: "TLSv1.2" }), ca: ROOT });
https.globalAgent.options.ca = ROOT;
await request("globalAgent.options.ca", port, {});
await request("globalAgent.options.ca + maxVersion 1.2", port, { maxVersion: "TLSv1.2" });
delete https.globalAgent.options.ca;
await request("globalAgent.options.ca removed", port, {});
await closeAll(server);

// ---- a server capped at 1.2: a floor of 1.3 is refused with the alert
server = await serve({ minVersion: "TLSv1.2", maxVersion: "TLSv1.2" });
port = server.address().port;
await request("server 1.2: ca", port, { ca: ROOT });
const base = { host: "localhost", port, ca: ROOT, minVersion: "TLSv1.3" };
function tlsCase(label, after) {
  section = label;
  return new Promise((resolve) => {
    const events = [];
    const s = tls.connect(base, () => { events.push("secureConnect"); s.destroy(); });
    s.on("error", (e) => { events.push("error " + shape(e)); });
    s.on("close", (hadError) => { events.push("close hadError=" + hadError); console.log(label + " -> " + events.join(" | ")); resolve(); });
    if (after) after(s, events);
  });
}
await tlsCase("tls.connect no write");
await tlsCase("tls.connect end() no data", (s) => s.end());
await tlsCase("tls.connect write", (s, events) => s.write("x", (e) => events.push("write cb " + (e ? e.code : "ok"))));
await tlsCase("tls.connect end(data)", (s, events) => s.end("x", () => events.push("end cb")));
await tlsCase("tls.connect two writes", (s, events) => {
  s.write("a", (e) => events.push("cb1 " + (e ? e.code : "ok")));
  s.write("b", (e) => events.push("cb2 " + (e ? e.code : "ok")));
});
await tlsCase("tls.connect write next tick", (s, events) => process.nextTick(() => s.write("x", (e) => events.push("write cb " + (e ? e.code : "ok")))));
function httpsCase(label, after) {
  section = label;
  return new Promise((resolve) => {
    const r = https.request({ ...base, path: "/" }, (res) => { console.log(label + " -> response"); res.resume(); resolve(); });
    r.on("error", (e) => { console.log(label + " -> ERROR " + shape(e)); resolve(); });
    if (after) after(r);
  });
}
await httpsCase("https.request end()", (r) => r.end());
await httpsCase("https.request never ended", () => {});
await httpsCase("https.request flushHeaders()", (r) => r.flushHeaders());
await httpsCase("https.request write()", (r) => r.write("x"));
await httpsCase("https.request end() after the refusal", (r) => setTimeout(() => r.end(), 400));
section = "https.get url";
await new Promise((resolve) => {
  // https.get ends the request itself: its head is queued.
  const r = https.get("https://localhost:" + port + "/", { ca: ROOT, minVersion: "TLSv1.3" }, (res) => { console.log("https.get url -> response"); res.resume(); resolve(); });
  r.on("error", (e) => { console.log("https.get url -> ERROR " + shape(e)); resolve(); });
});
await closeAll(server);

clearTimeout(watchdog);
