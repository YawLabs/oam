// socket.resetAndDestroy() off the connected path, as node v22.22.2 has it:
// the method's own shape (an enumerable prototype property, a function with
// no name, the same function on tls.TLSSocket); a socket with no handle is
// destroyed with ERR_SOCKET_CLOSED; one already destroyed is left alone (its
// peer sees the orderly end destroy() began); one still connecting is reset
// once it connects -- after the caller's own 'connect' listener -- or simply
// fails if the connect does; a TLS socket and a socket connecting to a pipe
// have no TCP handle, and the call throws ERR_INVALID_HANDLE_TYPE.
import net from "node:net";
import tls from "node:tls";

const out = [];
const log = (line) => out.push(line);
const describe = (e) => `${e.name} ${e.code} | ${e.message}`;

function record(socket) {
  const events = [];
  socket.on("end", () => events.push("end"));
  socket.on("error", (e) => events.push(`error ${e.code} | ${e.message.replace(/:\d+$/, ":PORT")}`));
  const closed = new Promise((resolve) =>
    socket.on("close", (...args) => {
      events.push(`close ${args.length === 0 ? "(no argument)" : args[0]}`);
      resolve();
    }));
  return { events, closed };
}

// The shape.
{
  const d = Object.getOwnPropertyDescriptor(net.Socket.prototype, "resetAndDestroy");
  log(`descriptor writable ${d.writable} enumerable ${d.enumerable} configurable ${d.configurable}`);
  log(`name ${JSON.stringify(d.value.name)} length ${d.value.length}`);
  log(`tls.TLSSocket shares it ${tls.TLSSocket.prototype.resetAndDestroy === d.value}`);
}

// No handle: never connected.
{
  log("--- new net.Socket()");
  const socket = new net.Socket();
  const rec = record(socket);
  let keys;
  socket.on("error", (e) => { keys = Object.keys(e); });
  log(`returns the socket ${socket.resetAndDestroy() === socket} destroyed ${socket.destroyed}`);
  await rec.closed;
  log(`events: ${rec.events.join(", ")}`);
  log(`error keys ${JSON.stringify(keys)}`);
}

const server = net.createServer();
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const port = server.address().port;
const nextPeer = () => new Promise((resolve) => server.once("connection", (conn) => resolve(record(conn))));

// Destroyed first: nothing more happens.
{
  log("--- destroy(), then resetAndDestroy()");
  const peerRec = nextPeer();
  const socket = net.connect(port, "127.0.0.1");
  const rec = record(socket);
  await new Promise((resolve) => socket.once("connect", resolve));
  const peer = await peerRec;
  socket.destroy();
  log(`returns the socket ${socket.resetAndDestroy() === socket}`);
  await Promise.all([rec.closed, peer.closed]);
  log(`client: ${rec.events.join(", ")}`);
  log(`peer:   ${peer.events.join(", ")}`);
}

// Still connecting (its name is still being looked up).
{
  log("--- while connecting");
  const peerRec = nextPeer();
  const socket = net.connect(port, "localhost");
  const rec = record(socket);
  socket.on("connect", () => rec.events.push(`connect (destroyed ${socket.destroyed})`));
  socket.on("ready", () => rec.events.push("ready"));
  log(`returns the socket ${socket.resetAndDestroy() === socket}`);
  log(`destroyed ${socket.destroyed} connecting ${socket.connecting} connect listeners ${socket.listenerCount("connect")}`);
  const peer = await peerRec;
  await Promise.all([rec.closed, peer.closed]);
  log(`client: ${rec.events.join(", ")}`);
  log(`peer:   ${peer.events.join(", ")}`);
}

// Connecting, and the connect fails: the reset never comes.
{
  log("--- while connecting, refused");
  const probe = net.createServer();
  await new Promise((resolve) => probe.listen(0, "127.0.0.1", resolve));
  const closedPort = probe.address().port;
  await new Promise((resolve) => probe.close(resolve));
  const socket = net.connect(closedPort, "127.0.0.1");
  const rec = record(socket);
  socket.resetAndDestroy();
  await rec.closed;
  log(`client: ${rec.events.join(", ")}`);
}

// Not a TCP handle.
{
  log("--- tls.TLSSocket");
  const socket = new tls.TLSSocket();
  try {
    socket.resetAndDestroy();
    log("did not throw");
  } catch (e) {
    log(`throws ${describe(e)} TypeError ${e instanceof TypeError}`);
  }
  log(`destroyed ${socket.destroyed}`);
  socket.destroy();
  log(`destroyed: returns the socket ${socket.resetAndDestroy() === socket}`);
}
{
  log("--- connecting to a pipe");
  const path = process.platform === "win32" ? "\\\\.\\pipe\\oam-conformance-253-absent" : "/oam-conformance-253-absent.sock";
  const socket = net.connect(path);
  socket.on("error", () => {});
  try {
    socket.resetAndDestroy();
    log("did not throw");
  } catch (e) {
    log(`throws ${describe(e)}`);
  }
  socket.destroy();
}

server.close();
process.stdout.write(out.join("\n") + "\n");
