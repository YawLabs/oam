// socket.ref() after unref() pins the event loop again (#140).
//
// Every handle is unref'd, then the client is ref'd back. An UNREF'D timer
// -- which fires only while something else keeps the process alive -- still
// runs, closes everything, and only then does the process exit. The order
// of the lines is the assertion: "timer" must come before "exit". Had ref()
// not re-pinned the loop, the process would have exited straight after the
// data line and the timer would never have fired.
import net from "node:net";

const lines = [];
process.on("exit", (code) => {
  lines.push("exit " + code);
  process.stdout.write(lines.join("\n") + "\n");
});

let accepted;
const server = net.createServer((conn) => {
  accepted = conn;
  conn.on("data", () => {});
  conn.write("hello");
  conn.unref();
});
server.listen(0, "127.0.0.1", () => {
  const client = net.connect(server.address().port, "127.0.0.1");
  client.on("data", (chunk) => {
    lines.push("data " + chunk);
    client.unref();
    server.unref();
    lines.push("ref chained " + (client.ref() === client));
    // Only the ref'd client is listed again.
    lines.push("active " + JSON.stringify(process.getActiveResourcesInfo().filter((t) => t.startsWith("TCP"))));
    setTimeout(() => {
      lines.push("timer");
      client.destroy();
      accepted.destroy();
      server.close();
    }, 300).unref();
  });
});
