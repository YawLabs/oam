// A server's socket paused in its 'connection' listener: what the peer sends
// waits for resume() -- a 'data' listener added meanwhile does not resume it --
// then flows, 'end' after it.
//
// Regression guard: oam starts an accepted socket's first read before
// 'connection' runs, and pause() only stopped the reads after that one, so
// the first chunk was emitted as 'data' while the socket was paused -- to no
// listener here, i.e. lost. Node v22.22.2 buffers it.
import net from "node:net";

setTimeout(() => {
  console.log("WATCHDOG");
  process.exit(9);
}, 30000).unref();

const log = [];
let peer;
const server = net.createServer((p) => {
  peer = p;
  p.on("error", (e) => log.push(`peer error ${e.code}`));
  p.on("end", () => log.push("peer end"));
  p.on("close", () => log.push("peer close"));
  p.pause();
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const client = net.connect(server.address().port, "127.0.0.1");
await new Promise((resolve) => client.once("connect", resolve));
client.write("one");
await new Promise((resolve) => setTimeout(resolve, 100));
log.push("paused, 'one' sent");
peer.on("data", (d) => log.push(`peer data ${d}`));
await new Promise((resolve) => setTimeout(resolve, 100));
log.push("paused, a 'data' listener added");
client.end("two");
await new Promise((resolve) => setTimeout(resolve, 100));
log.push("paused, 'two' and the FIN sent");
peer.resume();
log.push("resumed");
await new Promise((resolve) => peer.once("close", resolve));
await new Promise((resolve) => setTimeout(resolve, 50));
console.log(log.join("\n"));
server.close();

// A socket whose buffer filled up (paused mode, a 'readable' listener that
// does not read), then pause()d, switched back to 'data' and resume()d,
// reads the rest. (oam's read loop, stopped on the full buffer, was still
// unwinding when resume() asked for it again, and the socket never read
// past what it had buffered: no more 'data', no 'end'.)
{
  const total = 4 * 1024 * 1024;
  const done = [];
  const big = net.createServer((p) => {
    let got = 0;
    const idle = () => {};
    p.on("readable", idle);
    p.on("end", () => done.push(`end after ${got === total ? "every byte" : `${got} of ${total} bytes`}`));
    p.on("close", () => {
      done.push("close");
      big.close();
    });
    setTimeout(() => {
      p.pause();
      p.removeListener("readable", idle);
      p.on("data", (d) => { got += d.length; });
      done.push("resume");
      p.resume();
    }, 200);
  });
  await new Promise((resolve) => big.listen(0, "127.0.0.1", resolve));
  const c = net.connect(big.address().port, "127.0.0.1");
  c.on("error", () => {});
  c.end(Buffer.alloc(total, 7));
  await new Promise((resolve) => big.once("close", resolve));
  console.log(`a paused socket sent ${total} bytes: ${done.join(" | ")}`);
}
