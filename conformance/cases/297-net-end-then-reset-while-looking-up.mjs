// end(data) and then resetAndDestroy() on a socket still connecting to a
// name (a lookup in flight): node's reset, a 'connect' listener, runs before
// the end()'s shutdown -- which waits for the write ahead of it -- so the
// connection is reset: the server sees ECONNRESET and no FIN, the
// end() callback gets ERR_STREAM_DESTROYED and the client's 'close' says
// false, with no 'error' (measured on node v22.22.2). oam issued the held
// write and the FIN at the connect, and then refused the reset with
// EINVAL: the server got the data and a clean end. (Through an IP literal
// node itself refuses the reset with EINVAL, and oam does the same; that
// is not a case here, as node's client then never emits 'close'.)
import net from "node:net";

setTimeout(() => {
  console.log("WATCHDOG", clientLog.join(", "), "|", serverLog.join(", "));
  process.exit(9);
}, 30000).unref();

const clientLog = [];
const serverLog = [];
let serverClosed;
const done = new Promise((r) => { serverClosed = r; });
const server = net.createServer((s) => {
  // Whether the written bytes reach the server before the reset is the
  // scheduler's, in node too, so they are not printed.
  s.on("data", () => {});
  s.on("end", () => serverLog.push("end"));
  s.on("error", (e) => serverLog.push(`error ${e.code}`));
  s.on("close", () => {
    serverLog.push("close");
    serverClosed();
  });
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
let clientClosed;
const clientDone = new Promise((r) => { clientClosed = r; });
const c = net.connect(server.address().port, "localhost");
c.on("connect", () => clientLog.push("connect"));
c.on("error", (e) => clientLog.push(`error ${e.code}`));
c.on("close", (hadError) => {
  clientLog.push(`close ${hadError}`);
  clientClosed();
});
c.end("abc", (e) => clientLog.push(`end callback ${e ? e.code : null}`));
c.resetAndDestroy();
clientLog.push(`after the call: destroyed ${c.destroyed} connecting ${c.connecting}`);
await Promise.all([done, clientDone]);
server.close();
console.log("client:", clientLog.join(", "));
console.log("server:", serverLog.join(", "));
