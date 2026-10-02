// destroy() closes a socket's connection at once, whatever the peer is doing:
// a process whose last socket was destroyed exits, even when the peer never
// reads (a server socket paused in 'connection').
//
// Regression guard: oam's destroy() of a flowing socket left its parked read
// (and with it the descriptor) open until the peer answered the FIN. A
// paused peer never reads, so it never answered: the read stayed parked and
// the process never exited -- here the watchdog fired (exit 9). Node v22.22.2
// closes the handle in destroy() and exits.
import net from "node:net";

const watchdog = setTimeout(() => {
  console.log("WATCHDOG: the process is still alive");
  process.exit(9);
}, 10000);
watchdog.unref();

const server = net.createServer((peer) => {
  peer.on("error", () => {});
  peer.pause();
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const client = net.connect(server.address().port, "127.0.0.1");
await new Promise((resolve) => client.once("connect", resolve));
client.write("x");
await new Promise((resolve) => setTimeout(resolve, 100));
client.destroy();
await new Promise((resolve) => client.once("close", resolve));
server.close();
console.log("destroyed; the server socket is still open, paused:", !server.listening);
