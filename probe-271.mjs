// Probe: the last scenario of case 271 (upgrade, POST, Trailer, end('x')).
import http from "node:http";
import net from "node:net";

const V = "caféÿ";
const server = net.createServer((socket) => {
  let buf = Buffer.alloc(0);
  socket.on("data", (chunk) => {
    buf = Buffer.concat([buf, chunk]);
    const end = buf.indexOf("\r\n\r\n");
    if (end < 0) return;
    const line = buf.subarray(0, end).toString("latin1").split("\r\n").find((l) => /^x-v:/i.test(l)) || "(none)";
    console.log("head:", line);
    socket.end("HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
  });
  socket.on("error", () => {});
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const { port } = server.address();

const req = http.request(
  {
    port,
    host: "127.0.0.1",
    method: "POST",
    agent: false,
    headers: { connection: "upgrade", upgrade: "x-test", "x-v": V, trailer: "x-t" },
  },
  (res) => {
    console.log("response", res.statusCode);
    res.resume();
    res.on("end", () => console.log("res end"));
    res.on("close", () => console.log("res close"));
  },
);
req.on("error", (e) => console.log("req error", e.code));
req.on("close", () => console.log("req close"));
req.on("finish", () => console.log("req finish"));
req.end("x");

setTimeout(() => {
  console.log("watchdog: still alive at 5s");
  server.close();
  process.exit(1);
}, 5000);