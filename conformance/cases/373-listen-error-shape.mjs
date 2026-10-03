// A server's listen error is node's uvExceptionWithHostPort: its own keys
// in the order code, errno, syscall, address, port -- code first, where a
// connect error (ExceptionWithHostPort) has errno first.
//
// Regression guard: oam built every system error errno-first, so a listen
// error's Object.keys() (and util.inspect of it) read errno, code, ... .
import net from "node:net";
import http from "node:http";

const held = net.createServer();
await new Promise((r) => held.listen(0, "127.0.0.1", r));
const port = held.address().port;
for (const [label, server] of [["net", net.createServer()], ["http", http.createServer()]]) {
  const err = await new Promise((resolve) => {
    server.on("error", resolve);
    server.listen(port, "127.0.0.1");
  });
  console.log(label, Object.keys(err), err.code, err.syscall, err.address, err.port === port,
    err.message.split(String(port)).join("PORT"));
}
held.close();
const refused = await new Promise((resolve) => {
  net.connect(port, "127.0.0.1").on("error", resolve);
});
console.log("connect", Object.keys(refused));
