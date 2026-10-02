// fetch gives a connection 10 s to be connected and, for https, handshaken
// (#157): undici's connect timeout, after which the fetch fails with
// `TypeError: fetch failed` and a `ConnectTimeoutError` cause -- code
// `UND_ERR_CONNECT_TIMEOUT`, a message naming the host and port asked for and
// the timeout, and undici's brands. Measured on node v22.22.2.
//
// oam had no connect timeout: a connect that never finished waited for the
// operating system (21 s on Windows, about two minutes on Linux) and failed
// with `connect ETIMEDOUT`, and one stuck in a TLS handshake never failed.
//
// The server accepts and never answers the ClientHello, so the connect
// stalls without depending on an unroutable address. `https.request` to the
// same server has no connect timeout in node and is still waiting when the
// fetch gives up. This case takes the 10 s it tests; a dispatcher's shorter
// timeout is an e2e test, as no undici is importable here under node.
import https from "node:https";
import net from "node:net";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 60000).unref();

const sockets = new Set();
const server = net.createServer((sock) => {
  sockets.add(sock);
  sock.on("error", () => {});
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const port = server.address().port;
const scrub = (text) => String(text).replaceAll(String(port), "PORT");

let requestSettled = false;
const req = https.request({ host: "127.0.0.1", port, path: "/" }, () => {
  requestSettled = true;
});
req.on("error", () => {
  requestSettled = true;
});
req.end();

const started = Date.now();
try {
  const res = await fetch(`https://127.0.0.1:${port}/`);
  console.log("resolved", res.status);
} catch (e) {
  const c = e.cause;
  console.log(`${e.constructor.name}: ${e.message}`);
  console.log(`cause ${c?.constructor?.name} name=${c?.name} code=${c?.code}`);
  console.log(`message ${JSON.stringify(scrub(c?.message))}`);
  console.log(`own ${JSON.stringify(Reflect.ownKeys(c ?? {}).map(String))}`);
  console.log(`instanceof Error ${c instanceof Error}`);
  console.log(`brand ${c?.[Symbol.for("undici.error.UND_ERR_CONNECT_TIMEOUT")]}`);
}
const waited = Date.now() - started;
// undici's timer is coarse (it fires up to a second or so late), so only
// the near side is compared; the watchdog is the far side.
console.log(`not before the timeout ${waited >= 9000}`);
console.log(`https.request still waiting ${!requestSettled}`);
req.destroy();
for (const sock of sockets) sock.destroy();
server.close();
