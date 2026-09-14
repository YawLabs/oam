// A refused loopback connect fails at once (#137). Windows retransmits the
// SYN of a connect to a closed local port for about 2 s before reporting
// ECONNREFUSED; libuv tells the stack not to (SIO_TCP_INITIAL_RTO, loopback
// targets only), so node sees the refusal in milliseconds, and so must oam.
// The timing CLASS is what both runtimes print, so the case is byte-identical
// on every platform. Three shapes: an explicit 127.0.0.1 target, tls.connect's
// default host (localhost, which resolves to ::1 first on Windows), and
// net.connect to localhost reaching an IPv4-only listener through the refused
// ::1 attempt.
import net from "node:net";
import tls from "node:tls";

const timing = (since) => (Date.now() - since < 1000 ? "fast" : "slow");

// A port that was listening a moment ago and is closed now.
const probe = net.createServer();
probe.listen(0, "127.0.0.1", () => {
  const port = probe.address().port;
  probe.close(() => {
    const t1 = Date.now();
    const s = net.connect(port, "127.0.0.1");
    s.on("error", (e) => {
      console.log("net 127.0.0.1:", e.code, e.syscall, e.message.replace(String(port), "PORT"), timing(t1));
      const t2 = Date.now();
      const t = tls.connect({ port, rejectUnauthorized: false });
      t.on("error", (e2) => {
        console.log("tls default host:", e2.code, timing(t2));
        const listener = net.createServer((c) => c.end());
        listener.listen(0, "127.0.0.1", () => {
          const t3 = Date.now();
          const c = net.connect(listener.address().port, "localhost");
          c.on("connect", () => {
            console.log("net localhost -> IPv4 listener:", c.remoteAddress, timing(t3));
          });
          c.on("close", () => listener.close());
        });
      });
    });
  });
});
