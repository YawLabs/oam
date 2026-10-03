// http2.createSecureServer with allowHTTP1 closes its idle HTTP/1.1
// connections on close(), as node's does (httpServerPreClose, then
// tls.Server's close): a keep-alive client no longer holds the server --
// its 'close' comes once those connections are gone, not when the client
// lets go. closeIdleConnections() is on the prototype, and does nothing
// without allowHTTP1; closeAllConnections() is not there.
//
// Regression guard: oam's secure server had no close() of its own, so a
// keep-alive agent's connection -- or fetch's, which reaches such a server
// over HTTP/1.1 since #176 -- kept close() from finishing.
//
// Fixtures: conformance case 143's localhost leaf; the client skips
// verification (what is under test is the server).
import http2 from "node:http2";
import https from "node:https";

setTimeout(() => {
  console.log("WATCHDOG: close() waited for an idle keep-alive connection");
  process.exit(9);
}, 10000).unref();

const CERT = `-----BEGIN CERTIFICATE-----
MIIBvjCCAWWgAwIBAgIUOy7BLDqzc+0IZz2NWG95hnXgrd4wCgYIKoZIzj0EAwIw
GjEYMBYGA1UEAwwPb2FtIGgycyB0ZXN0IENBMCAXDTI1MDEwMTAwMDAwMFoYDzIx
MjUwMTAxMDAwMDAwWjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIB
BggqhkjOPQMBBwNCAATIZSROMPcNXcmsamcAQ6VM5NzCkR0bj0ngz5dpnyIRlajs
UptN/qPisRoVJ5BqZjfz4MS1vVN0KGg7vDRoCO1Vo4GMMIGJMBoGA1UdEQQTMBGC
CWxvY2FsaG9zdIcEfwAAATAJBgNVHRMEAjAAMAsGA1UdDwQEAwIHgDATBgNVHSUE
DDAKBggrBgEFBQcDATAdBgNVHQ4EFgQUmxnUU2rP4FwgoXrkCkeRxNgCVycwHwYD
VR0jBBgwFoAUOlIo6O4tIFNjD7vXJV51FU2DLQcwCgYIKoZIzj0EAwIDRwAwRAIg
ItB5f9aIsf9D8cXBvJvvr5ahB57RK7DgAsIVf5uJ0zcCIBPOR2Z+ycbeeByMKH2v
shKfeR1QdaoQHwJKJln0q1fo
-----END CERTIFICATE-----
`;
const KEY = `-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgQLidYpqFITu5wno8
Fw5b5Ahrg5eTwH0UqA7RU57egNKhRANCAATIZSROMPcNXcmsamcAQ6VM5NzCkR0b
j0ngz5dpnyIRlajsUptN/qPisRoVJ5BqZjfz4MS1vVN0KGg7vDRoCO1V
-----END PRIVATE KEY-----
`;

const plain = http2.createSecureServer({ cert: CERT, key: KEY });
const allow = http2.createSecureServer({ cert: CERT, key: KEY, allowHTTP1: true }, (req, res) => {
  res.end("served over " + req.httpVersion);
});
console.log("closeIdleConnections", typeof plain.closeIdleConnections, typeof allow.closeIdleConnections);
console.log("closeAllConnections", typeof allow.closeAllConnections);
console.log("own", Object.getOwnPropertyNames(Object.getPrototypeOf(allow)).join(","));
plain.closeIdleConnections();

await new Promise((r) => allow.listen(0, "127.0.0.1", r));
const agent = new https.Agent({ keepAlive: true });
const get = () => new Promise((resolve, reject) => {
  https.get({ host: "127.0.0.1", port: allow.address().port, path: "/", agent, rejectUnauthorized: false }, (res) => {
    let body = "";
    res.setEncoding("utf8");
    res.on("data", (d) => { body += d; });
    res.on("end", () => resolve(body));
  }).on("error", reject);
});
console.log(await get());
console.log(await get());
// The agent keeps its connection; the server's close() is what ends it.
await new Promise((resolve) => allow.close(() => resolve()));
console.log("closed");
agent.destroy();
