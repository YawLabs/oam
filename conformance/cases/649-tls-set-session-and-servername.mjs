// tls.TLSSocket.prototype.setSession / setServername and the `session`
// option as node v22.23.5 has them (lib/internal/tls/wrap.js). A session
// goes through getSessionForReuse (a string becomes latin1 bytes; a raw
// Buffer is dropped on a connection that verifies) to TLSWrap::SetSession,
// which throws its C++ TypeError "Session must be a buffer"
// (ERR_INVALID_ARG_TYPE, code only) for anything not an ArrayBufferView. A
// client socket's constructor hands a truthy `session` option to it
// directly, so tls.connect({ session: 42 }) throws after the secure context
// is built (a bad key or version wins) and before any connect is made --
// so before the servername block: no DEP0123, and ahead of a number
// servername's ERR_INVALID_ARG_TYPE. tls.connect then calls the socket's own
// setSession and setServername (validateString first, then
// ERR_TLS_SNI_FROM_SERVER on a server-side socket). Both methods are
// assigned functions: enumerable and unnamed. `servername` is null until the
// handshake sets it to the name setServername gave the handle, or false.
// oam had neither method and ignored `session`.
//
// EC fixture from case 103, self-signed; the client does not verify.
import tls from "node:tls";
import net from "node:net";

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

process.on("warning", (w) => console.log("warning", w.code));

function outcome(label, fn) {
  try {
    const r = fn();
    console.log(label, "ok", r === undefined ? "" : r);
  } catch (e) {
    console.log(label, "threw", e.name, e.code, e instanceof TypeError,
      JSON.stringify(Object.keys(e)), String(e.stack).split("\n")[0]);
  }
}

// The methods themselves.
for (const m of ["setSession", "setServername"]) {
  const d = Object.getOwnPropertyDescriptor(tls.TLSSocket.prototype, m);
  console.log(m, typeof d.value, d.enumerable, d.writable, d.configurable,
    JSON.stringify(d.value.name), d.value.length);
}

const values = [
  ["42", 42], ["null", null], ["undefined", undefined], ["object", {}],
  ["true", true], ["ArrayBuffer", new ArrayBuffer(3)], ["string", "abc"],
  ["Buffer", Buffer.from("not a session")], ["Uint8Array", new Uint8Array(2)],
  ["DataView", new DataView(new ArrayBuffer(2))],
];
// setSession on a socket that verifies (raw Buffers dropped) and on one
// that does not.
for (const opts of [{}, { rejectUnauthorized: false }]) {
  const s = new tls.TLSSocket(null, opts);
  console.log("socket", JSON.stringify(opts), "servername", s.servername);
  for (const [label, v] of values) outcome("  setSession " + label, () => s.setSession(v));
  s.destroy();
}
// The constructor's own check: clients only, no conversion.
for (const [label, v] of values) {
  outcome("new TLSSocket session " + label, () => { new tls.TLSSocket(null, { session: v }).destroy(); });
}
outcome("new server-side TLSSocket session 42", () => {
  new tls.TLSSocket(null, { session: 42, isServer: true }).destroy();
});
const serverSide = new tls.TLSSocket(null, { isServer: true });
outcome("server-side setSession 42", () => serverSide.setSession(42));

const bare = new tls.TLSSocket(null, {});
outcome("setServername 5", () => bare.setServername(5));
outcome("setServername undefined", () => bare.setServername());
outcome("setServername name", () => bare.setServername("x.test"));
console.log("after setServername", bare.servername);
outcome("server-side setServername name", () => serverSide.setServername("x.test"));
outcome("server-side setServername 5", () => serverSide.setServername(5));
bare.destroy();
serverSide.destroy();

const server = tls.createServer({ key: EC_KEY, cert: EC_CERT }, (s) => s.end());
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;
// The throwing connects below dial this one: none may reach it.
let watched = 0;
const watch = net.createServer((s) => { watched++; s.destroy(); });
await new Promise((r) => watch.listen(0, "127.0.0.1", r));
const watchPort = watch.address().port;

// tls.connect's order. A spy on the prototype shows it calls the socket's
// own methods, with what getSessionForReuse left.
const realSession = tls.TLSSocket.prototype.setSession;
const realName = tls.TLSSocket.prototype.setServername;
tls.TLSSocket.prototype.setSession = function (s) {
  console.log("  setSession called with", Buffer.isBuffer(s) ? "a Buffer" : typeof s);
  return realSession.call(this, s);
};
tls.TLSSocket.prototype.setServername = function (n) {
  console.log("  setServername called with", JSON.stringify(n));
  return realName.call(this, n);
};
const left = [];
for (const [label, extra] of [
  ["ip + 42", { servername: "1.1.1.1", session: 42, port: watchPort }],
  ["5 + 42", { servername: 5, session: 42, port: watchPort }],
  ["42 alone", { session: 42, port: watchPort }],
  ["bad minVersion + 42", { minVersion: "bad", session: 42, port: watchPort }],
  ["bad key + 42", { key: "nope", cert: "nope", session: 42, port: watchPort }],
  ["no port + 42", { port: undefined, session: 42 }],
  ["no port", { port: undefined }],
  ["0 session", { session: 0, servername: "" }],
  ["string session, verifying", { session: "abc", servername: "a.test", rejectUnauthorized: true }],
  ["string session", { session: "abc", servername: "a.test" }],
  ["Uint8Array session", { session: new Uint8Array(1) }],
  ["5 alone", { servername: 5 }],
]) {
  console.log(label);
  outcome("  tls.connect", () => {
    const s = tls.connect({ port, host: "127.0.0.1", rejectUnauthorized: false, ...extra });
    s.on("error", () => {});
    left.push(s);
    return "servername " + s.servername;
  });
}
console.log("over a socket: 5 + 42");
const raw = net.connect(port, "127.0.0.1");
raw.on("error", () => {});
outcome("  tls.connect", () => { tls.connect({ socket: raw, servername: 5, session: 42 }); });
console.log("over a socket: 5");
outcome("  tls.connect", () => { tls.connect({ socket: raw, servername: 5 }); });
raw.destroy();
tls.TLSSocket.prototype.setSession = realSession;
tls.TLSSocket.prototype.setServername = realName;
for (const s of left) s.destroy();
await new Promise((r) => setTimeout(r, 100));
console.log("connections made by the throwing connects", watched);
watch.close();

// `servername` once the handshake is done: the name setServername gave the
// handle, false without one.
async function handshake(label, make) {
  const s = make();
  await new Promise((resolve, reject) => {
    s.once("secureConnect", resolve);
    s.once("error", reject);
  });
  console.log(label, "client servername", JSON.stringify(s.servername));
  s.destroy();
}
const client = { port, rejectUnauthorized: false };
await handshake("servername option", () => tls.connect({ ...client, host: "127.0.0.1", servername: "a.test" }));
await handshake("no servername", () => tls.connect({ ...client, host: "localhost" }));
await handshake("empty servername", () => tls.connect({ ...client, host: "localhost", servername: "" }));
await handshake("setServername after tls.connect returns", () => {
  const s = tls.connect({ ...client, host: "localhost" });
  s.setServername("b.test");
  return s;
});
await handshake("over a socket", () =>
  tls.connect({ ...client, socket: net.connect(port, "127.0.0.1"), servername: "c.test" }));
server.close();
