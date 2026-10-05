// socket.resetAndDestroy() closes a TCP connection with a reset: the socket
// is destroyed at once and returned, its own 'close' says no error, and the
// peer's read fails with `read ECONNRESET` (errno, code, syscall) instead of
// ending. Client side and server side, a socket whose peer is not draining
// what it was sent, and a second call (a no-op on a destroyed socket). Each
// socket's events are kept apart and printed once both ends have closed:
// the order across the two ends is the scheduler's.
import net from "node:net";

const out = [];
const log = (line) => out.push(line);

function record(socket) {
  const events = [];
  socket.on("end", () => events.push("end"));
  socket.on("error", (e) =>
    events.push(`error ${e.code} | ${e.message} | syscall ${e.syscall} | errno ${typeof e.errno} | keys ${JSON.stringify(Object.keys(e))}`));
  const closed = new Promise((resolve) =>
    socket.on("close", (hadError) => {
      events.push(`close ${hadError}`);
      resolve();
    }));
  return { events, closed };
}

// The listening server's next accepted connection, recorded.
function accepting(server) {
  return new Promise((resolve) => server.once("connection", (conn) => resolve({ conn, rec: record(conn) })));
}

const server = net.createServer();
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const port = server.address().port;

async function connected() {
  const accepted = accepting(server);
  const client = net.connect(port, "127.0.0.1");
  const rec = record(client);
  await new Promise((resolve) => client.once("connect", resolve));
  const { conn, rec: peer } = await accepted;
  return { client, rec, conn, peer };
}

// 1. The client resets an idle connection.
{
  log("--- client resets");
  const { client, rec, peer } = await connected();
  const rv = client.resetAndDestroy();
  log(`returns the socket ${rv === client}`);
  log(`destroyed ${client.destroyed} readyState ${client.readyState} pending ${client.pending}`);
  log(`resetAndClosing ${client.resetAndClosing}`);
  log(`again returns the socket ${client.resetAndDestroy() === client}`);
  await Promise.all([rec.closed, peer.closed]);
  log(`client: ${rec.events.join(", ")}`);
  log(`peer:   ${peer.events.join(", ")}`);
}

// 2. The server resets the connection it accepted.
{
  log("--- server resets");
  const { rec, conn, peer } = await connected();
  conn.resetAndDestroy();
  await Promise.all([rec.closed, peer.closed]);
  log(`client: ${rec.events.join(", ")}`);
  log(`server: ${peer.events.join(", ")}`);
}

// 3. A peer that is not reading: what was written is still unsent, and the
// reset neither waits for it nor delivers it.
{
  log("--- reset with unsent data");
  const { client, rec, conn, peer } = await connected();
  conn.pause();
  let received = 0;
  conn.on("data", (chunk) => { received += chunk.length; });
  // More than any receive buffer holds: some of it is still the sender's.
  const big = Buffer.alloc(32 << 20, 1);
  client.write(big);
  client.resetAndDestroy();
  await rec.closed;
  conn.resume();
  // The peer learns of the reset on its next send. One that only reads may
  // never: macOS drops a reset that arrives against a closed window, and a
  // reader that drains and then waits sends nothing the closed port could
  // refuse -- measured under node v22.22.2 there as under oam, the step
  // hung past 4 s with the reset a few milliseconds behind the write. So
  // the peer writes, and which error ends it -- read ECONNRESET when the
  // reset got through, EPIPE from this write when it did not -- is the
  // kernel's timing, so only the close is reported.
  conn.write("x");
  await peer.closed;
  log(`client: ${rec.events.join(", ")}`);
  log(`peer:   closed with error ${peer.events.some((e) => e.startsWith("error"))}`);
  log(`peer got all of it ${received === big.length}`);
}

server.close();
process.stdout.write(out.join("\n") + "\n");
