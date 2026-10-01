// A WebSocket that cannot connect: the 'error' event is an ErrorEvent whose
// `message` is undici's fixed text and whose `error` is an Error carrying
// it, and a refused loopback port is reported at once.
//
// Regression guard (#161): oam's WebSocket dialled through its own plain
// TCP connect, outside the connector net, tls, fetch and http share, so on
// Windows a refused loopback port took ~2 s per resolved address (the SYN
// retransmit #137 removed everywhere else; `localhost` paid it twice), and
// the event was a bare Event with neither `message` nor `error`.
//
// The timing CLASS is what both runtimes print, as in case 101. The 'close'
// event is not logged: node v22 fires none for a connect that failed, oam
// fires the 1006 the WHATWG spec asks for (docs/node-divergences.md).
import net from "node:net";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

const timing = (since) => (Date.now() - since < 1000 ? "fast" : "slow");

// A loopback port nothing listens on.
async function closedPort() {
  const server = net.createServer();
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address();
  await new Promise((resolve) => server.close(resolve));
  return port;
}

function describe(ev) {
  const error = ev.error;
  return [
    `${ev.constructor.name} ${Object.prototype.toString.call(ev)}`,
    `type=${ev.type}`,
    `message=${JSON.stringify(ev.message)}`,
    `error=${error && error.constructor.name} ${error && JSON.stringify(error.message)}`,
    // The attributes are the prototype's getters, not own properties.
    `own=${JSON.stringify(["message", "filename", "lineno", "colno", "error"].filter((k) => Object.hasOwn(ev, k)))}`,
    `filename=${JSON.stringify(ev.filename)} lineno=${ev.lineno} colno=${ev.colno}`,
  ].join(" | ");
}

function failure(url) {
  return new Promise((resolve) => {
    const ws = new WebSocket(url);
    const seen = [];
    ws.onopen = () => seen.push("open");
    // The handler and a listener see the same event.
    let viaHandler = null;
    ws.onerror = (ev) => {
      viaHandler = ev;
    };
    ws.addEventListener("error", (ev) => {
      seen.push(`same event for onerror and the listener: ${ev === viaHandler}`);
      seen.push(describe(ev));
      resolve(seen);
    });
  });
}

for (const host of ["127.0.0.1", "localhost"]) {
  const port = await closedPort();
  const started = Date.now();
  const seen = await failure(`ws://${host}:${port}/`);
  console.log(`refused ${host}: ${timing(started)}`);
  for (const line of seen) console.log(`  ${line}`);
}

{
  // A server that answers the handshake with something other than a 101.
  const server = net.createServer((socket) => {
    socket.on("error", () => {});
    socket.once("data", () => socket.end("HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n"));
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const seen = await failure(`ws://127.0.0.1:${server.address().port}/`);
  console.log("a 404 answer:");
  for (const line of seen) console.log(`  ${line}`);
  server.close();
}

// Node v22 has the class and no global for it.
console.log("typeof ErrorEvent:", typeof ErrorEvent);
// Let a 'close' that follows the failure pass before leaving.
await new Promise((resolve) => setTimeout(resolve, 100));
console.log("done");
