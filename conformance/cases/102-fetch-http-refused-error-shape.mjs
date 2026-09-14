// A refused or unresolvable connection reports node's error shape through
// fetch() and http.request(). fetch rejects with the bare TypeError "fetch
// failed" and the transport error underneath as `cause`; http emits that
// error itself. Both carry errno / code / syscall and name the peer (address
// + port for connect, hostname for getaddrinfo). oam used to guess
// ECONNRESET from reqwest's text for http, and fetch carried no cause at all,
// so retry logic keyed on `code === 'ECONNREFUSED'` never fired. errno is
// platform-specific, and the differential runs node and oam on the same
// platform, so it is printed too.
import net from "node:net";
import http from "node:http";

const shape = (e, port) => JSON.stringify({
  name: e.constructor.name,
  message: e.message,
  keys: Object.keys(e),
  errno: e.errno, code: e.code, syscall: e.syscall,
  address: e.address, port: e.port, hostname: e.hostname,
}).replaceAll(String(port), "PORT");

// A port that was listening a moment ago and is closed now.
const probe = net.createServer();
probe.listen(0, "127.0.0.1", () => {
  const port = probe.address().port;
  probe.close(async () => {
    try {
      await fetch("http://127.0.0.1:" + port + "/");
      console.log("fetch 127.0.0.1: resolved?!");
    } catch (e) {
      console.log("fetch 127.0.0.1:", e.constructor.name, JSON.stringify(e.message), "own keys", JSON.stringify(Object.keys(e)));
      console.log("  cause:", shape(e.cause, port));
    }
    try {
      await fetch("http://[::1]:" + port + "/");
      console.log("fetch ::1: resolved?!");
    } catch (e) {
      console.log("fetch ::1 cause:", shape(e.cause, port));
    }
    try {
      await fetch("http://this.host.definitely.does.not.exist.invalid:" + port + "/");
      console.log("fetch unresolvable: resolved?!");
    } catch (e) {
      console.log("fetch unresolvable cause:", shape(e.cause, port));
    }
    const req = http.get({ host: "127.0.0.1", port }, () => console.log("http.get: response?!"));
    req.on("error", (e) => {
      console.log("http.get 127.0.0.1:", shape(e, port));
      const r2 = http.get("http://this.host.definitely.does.not.exist.invalid:" + port + "/", () => console.log("http.get: response?!"));
      r2.on("error", (e2) => console.log("http.get unresolvable:", shape(e2, port)));
    });
  });
});
