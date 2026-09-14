// socket.unref() / server.unref() release the event loop (#140).
//
// With every handle unref'd -- the client, the listener AND the accepted
// connection, which node keeps ref'd until told otherwise (probed on
// v22.22.2: an unref'd client + listener alone keep the process alive
// through the accepted socket) -- the process exits on its own with nothing
// closed and every socket still reading. oam used to keep running: the
// socket's parked read counted toward loop-liveness whatever the flag said.
//
// Both runtimes print the same lines and exit 0; a runtime that holds the
// loop times out here. Output is written from 'exit' so a hang shows as an
// absent verdict, not a partial one.
import net from "node:net";

const lines = [];
process.on("exit", (code) => {
  lines.push("exit " + code);
  process.stdout.write(lines.join("\n") + "\n");
});

const server = net.createServer((conn) => {
  conn.on("data", () => {});
  conn.write("hello");
  conn.unref();
});
server.listen(0, "127.0.0.1", () => {
  const client = net.connect(server.address().port, "127.0.0.1");
  client.on("data", (chunk) => {
    lines.push("data " + chunk);
    lines.push("readyState " + client.readyState + " pending " + client.pending);
    // Chainable, as node's are.
    lines.push("unref chained " + (client.unref() === client && server.unref() === server));
    // Unref'd handles leave the active-resource view.
    lines.push("active " + JSON.stringify(process.getActiveResourcesInfo().filter((t) => t.startsWith("TCP"))));
  });
});
