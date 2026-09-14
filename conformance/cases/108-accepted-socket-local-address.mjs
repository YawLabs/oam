// A server-side (accepted) net.Socket reports its own local address (#140
// follow-up). oam's accept op used to carry only the remote address, so an
// accepted socket's `address()` was `{}` and `localAddress` / `localPort` /
// `localFamily` were undefined, where Node fills them from the accepted
// socket's own end. Bound to 127.0.0.1 so both runtimes see IPv4 and the
// output is byte-identical on every platform; the dynamic port is compared,
// not printed.
import net from "node:net";

const server = net.createServer((conn) => {
  const a = conn.address();
  console.log("address keys:", JSON.stringify(Object.keys(a)));
  console.log("address.address:", a.address);
  console.log("address.family:", a.family);
  console.log("address.port is the listen port:", a.port === server.address().port);
  console.log("localAddress:", conn.localAddress);
  console.log("localFamily:", conn.localFamily);
  console.log("localPort is the listen port:", conn.localPort === server.address().port);
  conn.end();
});

server.listen(0, "127.0.0.1", () => {
  const c = net.connect(server.address().port, "127.0.0.1");
  c.on("data", () => {});
  c.on("end", () => {
    c.destroy();
    server.close();
  });
});
